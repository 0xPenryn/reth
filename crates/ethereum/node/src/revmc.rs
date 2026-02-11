//! revmc-backed Ethereum EVM factory.

use alloy_evm::{eth::EthEvmBuilder, precompiles::PrecompilesMap, Database, EvmEnv, EvmFactory};
use alloy_primitives::{Address, Bytes};
use alloy_primitives_08::B256 as RevmcB256;
use core::{
    mem,
    ops::{Deref, DerefMut},
};
use revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{
        result::{EVMError, HaltReason, ResultAndState},
        ContextSetters, ContextTr, FrameStack,
    },
    handler::{
        evm::{ContextDbError, EvmTr, FrameInitOrResult, FrameInitResult},
        instructions::{EthInstructions, InstructionProvider},
        EthFrame, MainnetHandler, PrecompileProvider,
    },
    inspector::NoOpInspector,
    interpreter::{interpreter::EthInterpreter, InterpreterResult, SharedMemory},
    Context, ExecuteEvm, InspectEvm, Inspector, MainBuilder, MainContext, SystemCallEvm,
};
use revmc_reth::{RevmcConfig, RevmcRuntime};
use std::sync::Arc;

type EthContext<DB> = Context<BlockEnv, TxEnv, CfgEnv, DB>;
type InnerEthEvm<DB, I> = revm::context::Evm<
    EthContext<DB>,
    I,
    EthInstructions<EthInterpreter, EthContext<DB>>,
    PrecompilesMap,
    EthFrame<EthInterpreter>,
>;
type InnerRunnerEvm<CTX, INSP, I, P> =
    revm::context::Evm<CTX, INSP, I, P, EthFrame<EthInterpreter>>;

#[inline]
fn to_revmc_b256(hash: revm::primitives::B256) -> RevmcB256 {
    RevmcB256::from_slice(hash.as_slice())
}

/// Ethereum EVM wrapper that routes frame execution through `revmc-reth`.
#[allow(missing_debug_implementations)]
pub struct RevmcEthEvm<DB: Database, I> {
    inner: InnerEthEvm<DB, I>,
    runtime: Arc<RevmcRuntime>,
    inspect: bool,
}

impl<DB: Database, I> RevmcEthEvm<DB, I> {
    fn transact_revmc(
        &mut self,
        tx: TxEnv,
    ) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>>
    where
        I: Inspector<EthContext<DB>>,
    {
        self.inner.ctx.set_tx(tx);
        let output_or_error = {
            let mut revmc = RevmcRunner { inner: &mut self.inner, runtime: &self.runtime };
            MainnetHandler::default().run(&mut revmc)
        };
        let state = self.inner.finalize();
        let output = output_or_error?;
        Ok(ResultAndState::new(output, state))
    }

    /// Returns runtime stats source for external monitoring.
    pub fn runtime(&self) -> &RevmcRuntime {
        &self.runtime
    }
}

impl<DB: Database, I> RevmcEthEvm<DB, I> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &EthContext<DB> {
        &self.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub const fn ctx_mut(&mut self) -> &mut EthContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I> Deref for RevmcEthEvm<DB, I> {
    type Target = EthContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for RevmcEthEvm<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I> alloy_evm::Evm for RevmcEthEvm<DB, I>
where
    DB: Database,
    I: Inspector<EthContext<DB>>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = revm::primitives::hardfork::SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &Self::BlockEnv {
        &self.block
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            self.inner.inspect_tx(tx)
        } else {
            self.transact_revmc(tx)
        }
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.system_call_with_caller(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } = self.inner.ctx;
        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (&self.inner.ctx.journaled_state.database, &self.inner.inspector, &self.inner.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.ctx.journaled_state.database,
            &mut self.inner.inspector,
            &mut self.inner.precompiles,
        )
    }
}

/// Factory producing [`RevmcEthEvm`] instances.
#[derive(Debug, Clone)]
pub struct RevmcEthEvmFactory {
    runtime: Arc<RevmcRuntime>,
}

impl RevmcEthEvmFactory {
    /// Creates a new factory with the provided revmc runtime config.
    pub fn new(config: RevmcConfig) -> Self {
        Self { runtime: Arc::new(RevmcRuntime::new(config)) }
    }
}

impl Default for RevmcEthEvmFactory {
    fn default() -> Self {
        Self::new(RevmcConfig::default())
    }
}

impl EvmFactory for RevmcEthEvmFactory {
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = RevmcEthEvm<DB, I>;
    type Context<DB: Database> = EthContext<DB>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = revm::primitives::hardfork::SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        RevmcEthEvm {
            inner: EthEvmBuilder::new(db, input).build().into_inner(),
            runtime: Arc::clone(&self.runtime),
            inspect: false,
        }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        RevmcEthEvm {
            inner: EthEvmBuilder::new(db, input).activate_inspector(inspector).build().into_inner(),
            runtime: Arc::clone(&self.runtime),
            inspect: true,
        }
    }
}

struct RevmcRunner<'a, CTX, INSP, I, P> {
    inner: &'a mut InnerRunnerEvm<CTX, INSP, I, P>,
    runtime: &'a RevmcRuntime,
}

impl<CTX, INSP, I, P> EvmTr for RevmcRunner<'_, CTX, INSP, I, P>
where
    CTX: ContextTr + revm::interpreter::Host,
    I: InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Context = CTX;
    type Instructions = I;
    type Precompiles = P;
    type Frame = EthFrame<EthInterpreter>;

    fn all(
        &self,
    ) -> (&Self::Context, &Self::Instructions, &Self::Precompiles, &FrameStack<Self::Frame>) {
        (&self.inner.ctx, &self.inner.instruction, &self.inner.precompiles, &self.inner.frame_stack)
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        (
            &mut self.inner.ctx,
            &mut self.inner.instruction,
            &mut self.inner.precompiles,
            &mut self.inner.frame_stack,
        )
    }

    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as revm::handler::evm::FrameTr>::FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        <InnerRunnerEvm<CTX, INSP, I, P> as EvmTr>::frame_init(self.inner, frame_input)
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let frame = self.inner.frame_stack.get();
        let context = &mut self.inner.ctx;
        let instructions = &mut self.inner.instruction;

        let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
        let revmc_bytecode_hash = to_revmc_b256(bytecode_hash);
        let spec_id = frame.interpreter.runtime_flag.spec_id;

        let action = if let Some(compiled) = self.runtime.get_compiled(revmc_bytecode_hash) {
            let mut memory = mem::replace(&mut frame.interpreter.memory, SharedMemory::invalid());
            let action = unsafe {
                compiled.call_with_interpreter_and_memory(
                    &mut frame.interpreter,
                    &mut memory,
                    context,
                )
            };
            frame.interpreter.memory = memory;

            self.runtime.record_execution(
                revmc_bytecode_hash,
                &[],
                frame.interpreter.gas.spent(),
                spec_id,
            );

            action
        } else {
            let action = frame.interpreter.run_plain(instructions.instruction_table(), context);

            self.runtime.record_execution(
                revmc_bytecode_hash,
                frame.interpreter.bytecode.original_byte_slice(),
                frame.interpreter.gas.spent(),
                spec_id,
            );

            action
        };

        frame.process_next_action(context, action).inspect(|i| {
            if i.is_result() {
                frame.set_finished(true);
            }
        })
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as revm::handler::evm::FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as revm::handler::evm::FrameTr>::FrameResult>,
        ContextDbError<Self::Context>,
    > {
        <InnerRunnerEvm<CTX, INSP, I, P> as EvmTr>::frame_return_result(self.inner, result)
    }
}

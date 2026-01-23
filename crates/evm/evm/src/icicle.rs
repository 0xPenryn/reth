//! Icicle-enabled EVM factory wrappers.

use alloy_evm::{
    precompiles::{DynPrecompile, Precompile, PrecompilesMap},
    Evm, EvmEnv, EvmFactory,
};
use alloy_evm::revm::{inspector::NoOpInspector, Inspector};
use alloy_primitives::Address;
use reth_icicle as icicle;

/// EVM factory wrapper that can inject Icicle-accelerated precompiles.
#[derive(Debug, Clone, Copy, Default)]
pub struct IcicleEvmFactory<F> {
    inner: F,
}

impl<F> IcicleEvmFactory<F> {
    /// Create a new wrapper around the inner EVM factory.
    pub const fn new(inner: F) -> Self {
        Self { inner }
    }
}

impl<F> EvmFactory for IcicleEvmFactory<F>
where
    F: EvmFactory<Precompiles = PrecompilesMap>,
{
    type Evm<DB: alloy_evm::Database, I: Inspector<Self::Context<DB>>> = F::Evm<DB, I>;
    type Context<DB: alloy_evm::Database> = F::Context<DB>;
    type Tx = F::Tx;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = F::Error<DBError>;
    type HaltReason = F::HaltReason;
    type Spec = F::Spec;
    type BlockEnv = F::BlockEnv;
    type Precompiles = F::Precompiles;

    fn create_evm<DB: alloy_evm::Database>(
        &self,
        db: DB,
        evm_env: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let mut evm = self.inner.create_evm(db, evm_env);
        maybe_wrap_bn254_precompiles(evm.precompiles_mut());
        evm
    }

    fn create_evm_with_inspector<DB, I>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I>
    where
        DB: alloy_evm::Database,
        I: Inspector<Self::Context<DB>>,
    {
        let mut evm = self.inner.create_evm_with_inspector(db, input, inspector);
        maybe_wrap_bn254_precompiles(evm.precompiles_mut());
        evm
    }
}

fn maybe_wrap_bn254_precompiles(precompiles: &mut PrecompilesMap) {
    if !icicle::precompiles_enabled() {
        return;
    }

    let bn254_addresses = [precompile_address(6), precompile_address(7), precompile_address(8)];

    for address in bn254_addresses {
        precompiles.map_precompile(&address, |original| {
            let id = original.precompile_id().clone();
            DynPrecompile::new(id, move |input| {
                // TODO: route to Icicle bn254 operations when available.
                original.call(input)
            })
        });
    }
}

fn precompile_address(last_byte: u8) -> Address {
    let mut bytes = [0u8; 20];
    bytes[19] = last_byte;
    Address::from_slice(&bytes)
}

//! Icicle-enabled EVM factory wrappers.

use alloy_evm::{
    precompiles::{DynPrecompile, Precompile, PrecompilesMap},
    Evm, EvmEnv, EvmFactory,
};
use alloy_evm::revm::{
    inspector::NoOpInspector,
    precompile::{PrecompileError, PrecompileOutput},
    primitives::hardfork::SpecId,
    Inspector,
};
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
    F::Spec: Clone + Into<SpecId> + Bn254PairingInputLimit,
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
        let spec = evm_env.spec_id().clone();
        let mut evm = self.inner.create_evm(db, evm_env);
        maybe_wrap_bn254_precompiles(evm.precompiles_mut(), spec);
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
        let spec = input.spec_id().clone();
        let mut evm = self.inner.create_evm_with_inspector(db, input, inspector);
        maybe_wrap_bn254_precompiles(evm.precompiles_mut(), spec);
        evm
    }
}

fn maybe_wrap_bn254_precompiles<S>(precompiles: &mut PrecompilesMap, spec: S)
where
    S: Into<SpecId> + Clone + Bn254PairingInputLimit,
{
    if !icicle::precompiles_enabled() {
        return;
    }

    let spec_id: SpecId = spec.clone().into();
    let is_istanbul = spec_id >= SpecId::ISTANBUL;
    let pair_max_input = spec.bn254_pairing_max_input();

    let bn254_addresses = [
        (precompile_address(6), Bn254Kind::Add),
        (precompile_address(7), Bn254Kind::Mul),
        (precompile_address(8), Bn254Kind::Pair),
    ];

    for (address, kind) in bn254_addresses {
        precompiles.map_precompile(&address, move |original| {
            let id = original.precompile_id().clone();
            let fallback = original.clone();
            let kind = kind;
            DynPrecompile::new(id, move |input| {
                match run_bn254_precompile(kind, input.data, input.gas, is_istanbul, pair_max_input)
                {
                    Ok(output) => Ok(output),
                    Err(Bn254PrecompileError::Precompile(err)) => Err(err),
                    Err(Bn254PrecompileError::Backend) => fallback.call(input),
                }
            })
        });
    }
}

#[derive(Clone, Copy, Debug)]
enum Bn254Kind {
    Add,
    Mul,
    Pair,
}

#[derive(Debug)]
enum Bn254PrecompileError {
    Precompile(PrecompileError),
    Backend,
}

fn run_bn254_precompile(
    kind: Bn254Kind,
    input: &[u8],
    gas_limit: u64,
    is_istanbul: bool,
    pair_max_input: Option<usize>,
) -> Result<PrecompileOutput, Bn254PrecompileError> {
    match kind {
        Bn254Kind::Add => run_bn254_add(input, gas_limit, is_istanbul),
        Bn254Kind::Mul => run_bn254_mul(input, gas_limit, is_istanbul),
        Bn254Kind::Pair => run_bn254_pair(input, gas_limit, is_istanbul, pair_max_input),
    }
}

const FQ_LEN: usize = 32;
const SCALAR_LEN: usize = 32;
const G1_LEN: usize = 2 * FQ_LEN;
const G2_LEN: usize = 4 * FQ_LEN;
const ADD_INPUT_LEN: usize = 2 * G1_LEN;
const MUL_INPUT_LEN: usize = G1_LEN + SCALAR_LEN;
const PAIR_ELEMENT_LEN: usize = G1_LEN + G2_LEN;

const ISTANBUL_ADD_GAS_COST: u64 = 150;
const BYZANTIUM_ADD_GAS_COST: u64 = 500;
const ISTANBUL_MUL_GAS_COST: u64 = 6_000;
const BYZANTIUM_MUL_GAS_COST: u64 = 40_000;
const ISTANBUL_PAIR_PER_POINT: u64 = 34_000;
const ISTANBUL_PAIR_BASE: u64 = 45_000;
const BYZANTIUM_PAIR_PER_POINT: u64 = 80_000;
const BYZANTIUM_PAIR_BASE: u64 = 100_000;

#[inline]
fn right_pad<const N: usize>(input: &[u8]) -> [u8; N] {
    let mut padded = [0u8; N];
    let len = input.len().min(N);
    padded[..len].copy_from_slice(&input[..len]);
    padded
}

fn map_bn254_error(err: icicle::Bn254Error) -> Bn254PrecompileError {
    match err {
        icicle::Bn254Error::InvalidFieldElement => {
            Bn254PrecompileError::Precompile(PrecompileError::Bn254FieldPointNotAMember)
        }
        icicle::Bn254Error::InvalidPoint => {
            Bn254PrecompileError::Precompile(PrecompileError::Bn254AffineGFailedToCreate)
        }
        icicle::Bn254Error::Backend(_) => Bn254PrecompileError::Backend,
    }
}

fn run_bn254_add(
    input: &[u8],
    gas_limit: u64,
    is_istanbul: bool,
) -> Result<PrecompileOutput, Bn254PrecompileError> {
    let gas_cost = if is_istanbul { ISTANBUL_ADD_GAS_COST } else { BYZANTIUM_ADD_GAS_COST };
    if gas_cost > gas_limit {
        return Err(Bn254PrecompileError::Precompile(PrecompileError::OutOfGas));
    }

    let input = right_pad::<ADD_INPUT_LEN>(input);
    let p1_bytes = &input[..G1_LEN];
    let p2_bytes = &input[G1_LEN..];
    let output = icicle::bn254_g1_add(p1_bytes, p2_bytes).map_err(map_bn254_error)?;
    Ok(PrecompileOutput::new(gas_cost, output.to_vec().into()))
}

fn run_bn254_mul(
    input: &[u8],
    gas_limit: u64,
    is_istanbul: bool,
) -> Result<PrecompileOutput, Bn254PrecompileError> {
    let gas_cost = if is_istanbul { ISTANBUL_MUL_GAS_COST } else { BYZANTIUM_MUL_GAS_COST };
    if gas_cost > gas_limit {
        return Err(Bn254PrecompileError::Precompile(PrecompileError::OutOfGas));
    }

    let input = right_pad::<MUL_INPUT_LEN>(input);
    let point_bytes = &input[..G1_LEN];
    let scalar_bytes = &input[G1_LEN..G1_LEN + SCALAR_LEN];
    let output = icicle::bn254_g1_mul(point_bytes, scalar_bytes).map_err(map_bn254_error)?;
    Ok(PrecompileOutput::new(gas_cost, output.to_vec().into()))
}

fn run_bn254_pair(
    input: &[u8],
    gas_limit: u64,
    is_istanbul: bool,
    max_input: Option<usize>,
) -> Result<PrecompileOutput, Bn254PrecompileError> {
    if let Some(max) = max_input {
        if input.len() > max {
            return Err(Bn254PrecompileError::Precompile(PrecompileError::Bn254PairLength));
        }
    }

    let (pair_per_point, pair_base) = if is_istanbul {
        (ISTANBUL_PAIR_PER_POINT, ISTANBUL_PAIR_BASE)
    } else {
        (BYZANTIUM_PAIR_PER_POINT, BYZANTIUM_PAIR_BASE)
    };

    let gas_used = (input.len() / PAIR_ELEMENT_LEN) as u64 * pair_per_point + pair_base;
    if gas_used > gas_limit {
        return Err(Bn254PrecompileError::Precompile(PrecompileError::OutOfGas));
    }

    if !input.len().is_multiple_of(PAIR_ELEMENT_LEN) {
        return Err(Bn254PrecompileError::Precompile(PrecompileError::Bn254PairLength));
    }

    let elements = input.len() / PAIR_ELEMENT_LEN;
    let mut pairs = Vec::with_capacity(elements);
    for idx in 0..elements {
        let start = idx * PAIR_ELEMENT_LEN;
        let g1_start = start;
        let g2_start = start + G1_LEN;
        let g1 = &input[g1_start..g2_start];
        let g2 = &input[g2_start..g2_start + G2_LEN];
        pairs.push((g1, g2));
    }

    let pairing_result = icicle::bn254_pairing_check(&pairs).map_err(map_bn254_error)?;
    let mut output = [0u8; 32];
    if pairing_result {
        output[31] = 1;
    }
    Ok(PrecompileOutput::new(gas_used, output.to_vec().into()))
}

trait Bn254PairingInputLimit {
    fn bn254_pairing_max_input(self) -> Option<usize>;
}

impl Bn254PairingInputLimit for SpecId {
    fn bn254_pairing_max_input(self) -> Option<usize> {
        None
    }
}

#[cfg(feature = "op")]
impl Bn254PairingInputLimit for op_revm::OpSpecId {
    fn bn254_pairing_max_input(self) -> Option<usize> {
        use op_revm::precompiles::bn254_pair::{GRANITE_MAX_INPUT_SIZE, JOVIAN_MAX_INPUT_SIZE};
        use op_revm::OpSpecId;

        if self.is_enabled_in(OpSpecId::JOVIAN) {
            Some(JOVIAN_MAX_INPUT_SIZE)
        } else if self.is_enabled_in(OpSpecId::GRANITE) {
            Some(GRANITE_MAX_INPUT_SIZE)
        } else {
            None
        }
    }
}

fn precompile_address(last_byte: u8) -> Address {
    let mut bytes = [0u8; 20];
    bytes[19] = last_byte;
    Address::from_slice(&bytes)
}

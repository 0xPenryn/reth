//! Icicle GPU acceleration helpers.

use alloy_primitives::{keccak256, B256};
use reth_config::config::{IcicleBackend, IcicleConfig};
use reth_trie_common::{HashedPostState, HashedStorage, KeccakKeyHasher};
use revm_database::BundleState;
use std::sync::OnceLock;
#[cfg(feature = "icicle")]
use std::{
    sync::{atomic::AtomicU64, atomic::Ordering, Mutex},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

/// Icicle helper error wrapper.
#[derive(Debug, thiserror::Error)]
#[error("icicle error: {0}")]
pub struct IcicleError(String);

/// BN254 precompile error classification.
#[derive(Debug, thiserror::Error)]
pub enum Bn254Error {
    /// Input bytes are not a valid field element.
    #[error("bn254 invalid field element")]
    InvalidFieldElement,
    /// Input point is not on curve or not in the correct subgroup.
    #[error("bn254 invalid curve point")]
    InvalidPoint,
    /// Icicle backend error.
    #[error("bn254 backend error: {0}")]
    Backend(String),
}

#[derive(Debug, Clone)]
struct IcicleState {
    config: IcicleConfig,
    available: bool,
}

static ICICLE_STATE: OnceLock<IcicleState> = OnceLock::new();
#[cfg(feature = "icicle")]
static ICICLE_STATS: OnceLock<IcicleStats> = OnceLock::new();

#[cfg(feature = "icicle")]
const ICICLE_STATS_LOG_INTERVAL: Duration = Duration::from_secs(60);

#[cfg(feature = "icicle")]
#[derive(Debug, Clone, Copy)]
enum IcicleCallKind {
    Hash,
    Bn254,
}

#[cfg(feature = "icicle")]
#[derive(Debug)]
struct IcicleSnapshot {
    total_calls: u64,
    total_ns: u64,
    hash_calls: u64,
    hash_ns: u64,
    bn254_calls: u64,
    bn254_ns: u64,
}

#[cfg(feature = "icicle")]
#[derive(Debug)]
struct IcicleStats {
    start: Instant,
    last_log_ns: AtomicU64,
    total_calls: AtomicU64,
    total_ns: AtomicU64,
    hash_calls: AtomicU64,
    hash_ns: AtomicU64,
    bn254_calls: AtomicU64,
    bn254_ns: AtomicU64,
    snapshot: Mutex<IcicleSnapshot>,
}

#[cfg(feature = "icicle")]
impl IcicleStats {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last_log_ns: AtomicU64::new(0),
            total_calls: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            hash_calls: AtomicU64::new(0),
            hash_ns: AtomicU64::new(0),
            bn254_calls: AtomicU64::new(0),
            bn254_ns: AtomicU64::new(0),
            snapshot: Mutex::new(IcicleSnapshot {
                total_calls: 0,
                total_ns: 0,
                hash_calls: 0,
                hash_ns: 0,
                bn254_calls: 0,
                bn254_ns: 0,
            }),
        }
    }
}

#[cfg(feature = "icicle")]
fn record_icicle_call(kind: IcicleCallKind, elapsed: Duration) {
    let stats = ICICLE_STATS.get_or_init(IcicleStats::new);
    let elapsed_ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;

    stats.total_calls.fetch_add(1, Ordering::Relaxed);
    stats.total_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
    match kind {
        IcicleCallKind::Hash => {
            stats.hash_calls.fetch_add(1, Ordering::Relaxed);
            stats.hash_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
        }
        IcicleCallKind::Bn254 => {
            stats.bn254_calls.fetch_add(1, Ordering::Relaxed);
            stats.bn254_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
        }
    }

    let now_ns = stats.start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let last_ns = stats.last_log_ns.load(Ordering::Relaxed);
    if now_ns.saturating_sub(last_ns) < ICICLE_STATS_LOG_INTERVAL.as_nanos() as u64 {
        return;
    }

    if stats
        .last_log_ns
        .compare_exchange(last_ns, now_ns, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let totals = IcicleSnapshot {
        total_calls: stats.total_calls.load(Ordering::Relaxed),
        total_ns: stats.total_ns.load(Ordering::Relaxed),
        hash_calls: stats.hash_calls.load(Ordering::Relaxed),
        hash_ns: stats.hash_ns.load(Ordering::Relaxed),
        bn254_calls: stats.bn254_calls.load(Ordering::Relaxed),
        bn254_ns: stats.bn254_ns.load(Ordering::Relaxed),
    };

    let mut snapshot = stats.snapshot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let delta = IcicleSnapshot {
        total_calls: totals.total_calls.saturating_sub(snapshot.total_calls),
        total_ns: totals.total_ns.saturating_sub(snapshot.total_ns),
        hash_calls: totals.hash_calls.saturating_sub(snapshot.hash_calls),
        hash_ns: totals.hash_ns.saturating_sub(snapshot.hash_ns),
        bn254_calls: totals.bn254_calls.saturating_sub(snapshot.bn254_calls),
        bn254_ns: totals.bn254_ns.saturating_sub(snapshot.bn254_ns),
    };
    *snapshot = totals;

    if delta.total_calls == 0 {
        return;
    }

    info!(
        target: "reth::icicle",
        interval_ms = ICICLE_STATS_LOG_INTERVAL.as_millis() as u64,
        total_calls = delta.total_calls,
        total_ms = delta.total_ns / 1_000_000,
        hash_calls = delta.hash_calls,
        hash_ms = delta.hash_ns / 1_000_000,
        bn254_calls = delta.bn254_calls,
        bn254_ms = delta.bn254_ns / 1_000_000,
        "Icicle usage summary",
    );
}

/// Initialize the Icicle runtime if enabled in config.
///
/// Returns true if Icicle runtime is available and enabled.
pub fn init(config: &IcicleConfig) -> bool {
    if !config.enabled {
        return false;
    }

    if let Some(state) = ICICLE_STATE.get() {
        return state.available;
    }

    info!(
        target: "reth::icicle",
        backend = ?config.backend,
        device = config.device,
        "Icicle enabled",
    );
    let state = match init_runtime(config) {
        Ok(()) => {
            debug!(target: "reth::icicle", backend = ?config.backend, device = ?config.device, "Icicle runtime initialized");
            IcicleState { config: config.clone(), available: true }
        }
        Err(err) => {
            warn!(target: "reth::icicle", %err, "Icicle runtime unavailable, falling back to CPU");
            IcicleState { config: config.clone(), available: false }
        }
    };

    let _ = ICICLE_STATE.set(state);
    ICICLE_STATE.get().map_or(false, |state| state.available)
}

/// Returns true if Icicle is enabled and available.
pub fn available() -> bool {
    ICICLE_STATE.get().is_some_and(|state| state.available && state.config.enabled)
}

/// Returns true if hashing offload is enabled and available.
pub fn hashing_enabled() -> bool {
    ICICLE_STATE
        .get()
        .is_some_and(|state| state.available && state.config.enabled && state.config.hashing)
}

/// Returns true if state root offload is enabled and available.
pub fn state_root_enabled() -> bool {
    ICICLE_STATE
        .get()
        .is_some_and(|state| state.available && state.config.enabled && state.config.state_root)
}

/// Returns true if precompile acceleration is enabled and available.
pub fn precompiles_enabled() -> bool {
    ICICLE_STATE
        .get()
        .is_some_and(|state| state.available && state.config.enabled && state.config.precompiles)
}

/// Returns a suggested worker chunk size for hashing stages.
pub fn hashing_chunk_size_hint(default_size: usize) -> usize {
    if let Some(state) = ICICLE_STATE.get() {
        if state.available && state.config.enabled && state.config.hashing {
            return default_size.max(state.config.min_batch);
        }
    }
    default_size
}

/// Returns true if batch size meets Icicle offload threshold.
pub fn should_use_keccak_batch(batch: usize) -> bool {
    if let Some(state) = ICICLE_STATE.get() {
        return state.available
            && state.config.enabled
            && state.config.hashing
            && batch >= state.config.min_batch;
    }
    false
}

/// Returns true if batch size meets Icicle offload threshold for state root operations.
pub fn should_use_keccak_batch_state_root(batch: usize) -> bool {
    if let Some(state) = ICICLE_STATE.get() {
        return state.available
            && state.config.enabled
            && state.config.state_root
            && batch >= state.config.min_batch;
    }
    false
}

/// Compute Keccak-256 for a fixed-size batch using Icicle when available, falling back to CPU.
pub fn keccak256_batch_fixed_or_cpu(inputs: &[u8], item_len: usize) -> Vec<B256> {
    if inputs.is_empty() || item_len == 0 {
        return Vec::new();
    }
    let batch = inputs.len() / item_len;
    if inputs.len() % item_len != 0 {
        return inputs.chunks(item_len).map(keccak256).collect();
    }

    if should_use_keccak_batch(batch) {
        if let Ok(hashes) = keccak256_batch_fixed_gpu(inputs, item_len) {
            return hashes;
        }
    }

    inputs.chunks(item_len).map(keccak256).collect()
}

/// Compute Keccak-256 for a fixed-size batch, always attempting Icicle when hashing is enabled.
pub fn keccak256_batch_fixed_or_cpu_force(inputs: &[u8], item_len: usize) -> Vec<B256> {
    if inputs.is_empty() || item_len == 0 {
        return Vec::new();
    }
    let batch = inputs.len() / item_len;
    if inputs.len() % item_len != 0 {
        return inputs.chunks(item_len).map(keccak256).collect();
    }

    if hashing_enabled() && batch > 0 {
        if let Ok(hashes) = keccak256_batch_fixed_gpu(inputs, item_len) {
            return hashes;
        }
    }

    inputs.chunks(item_len).map(keccak256).collect()
}

/// Compute Keccak-256 for a fixed-size batch using Icicle for state root work, falling back to CPU.
pub fn keccak256_batch_fixed_or_cpu_state_root(inputs: &[u8], item_len: usize) -> Vec<B256> {
    if inputs.is_empty() || item_len == 0 {
        return Vec::new();
    }
    let batch = inputs.len() / item_len;
    if inputs.len() % item_len != 0 {
        return inputs.chunks(item_len).map(keccak256).collect();
    }

    if should_use_keccak_batch_state_root(batch) {
        if let Ok(hashes) = keccak256_batch_fixed_gpu(inputs, item_len) {
            return hashes;
        }
    }

    inputs.chunks(item_len).map(keccak256).collect()
}

/// Hash bundle state into a [`HashedPostState`], using Icicle for batching when enabled.
pub fn hashed_post_state_from_bundle_state(bundle_state: &BundleState) -> HashedPostState {
    if !state_root_enabled() {
        return HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state());
    }

    let state = bundle_state.state();
    if state.is_empty() {
        return HashedPostState::default();
    }

    let account_len = state.len();
    let total_slots = state.values().map(|account| account.storage.len()).sum::<usize>();

    if !should_use_keccak_batch_state_root(account_len) && !should_use_keccak_batch_state_root(total_slots) {
        return HashedPostState::from_bundle_state::<KeccakKeyHasher>(state);
    }

    const ADDRESS_LEN: usize = 20;
    const SLOT_LEN: usize = 32;

    struct StorageMeta {
        start: usize,
        len: usize,
        wiped: bool,
    }

    let mut address_bytes = Vec::with_capacity(account_len * ADDRESS_LEN);
    let mut account_infos = Vec::with_capacity(account_len);
    let mut storage_meta = Vec::with_capacity(account_len);

    let mut slot_bytes = Vec::with_capacity(total_slots * SLOT_LEN);
    let mut slot_values = Vec::with_capacity(total_slots);

    for (address, account) in state {
        address_bytes.extend_from_slice(address.as_ref());
        account_infos.push(account.info.as_ref().map(Into::into));

        let start = slot_values.len();
        for (slot, value) in account.storage.iter() {
            slot_values.push(value.present_value);
            let slot_b256 = B256::from(*slot);
            slot_bytes.extend_from_slice(slot_b256.as_ref());
        }
        let len = slot_values.len() - start;
        storage_meta.push(StorageMeta { start, len, wiped: account.status.was_destroyed() });
    }

    let hashed_addresses = keccak256_batch_fixed_or_cpu_state_root(&address_bytes, ADDRESS_LEN);
    let hashed_slots = keccak256_batch_fixed_or_cpu_state_root(&slot_bytes, SLOT_LEN);

    let mut hashed_state = HashedPostState::with_capacity(account_len);
    for ((hashed_address, hashed_account), meta) in
        hashed_addresses.into_iter().zip(account_infos.into_iter()).zip(storage_meta.iter())
    {
        hashed_state.accounts.insert(hashed_address, hashed_account);

        if meta.len > 0 || meta.wiped {
            let iter = hashed_slots[meta.start..meta.start + meta.len]
                .iter()
                .copied()
                .zip(slot_values[meta.start..meta.start + meta.len].iter().copied());
            let hashed_storage = HashedStorage::from_iter(meta.wiped, iter);
            if !hashed_storage.is_empty() {
                hashed_state.storages.insert(hashed_address, hashed_storage);
            }
        }
    }

    hashed_state
}

fn backend_env_value(backend: IcicleBackend) -> Option<&'static str> {
    match backend {
        IcicleBackend::Auto => None,
        IcicleBackend::Cuda => Some("CUDA"),
        IcicleBackend::Metal => Some("METAL"),
    }
}

#[cfg(feature = "icicle")]
fn init_runtime(config: &IcicleConfig) -> Result<(), IcicleError> {
    if let Some(backend) = backend_env_value(config.backend) {
        // SAFETY: We only set env vars before Icicle runtime init; this is process-global and
        // mirrors how Icicle is configured via env in standalone binaries.
        unsafe {
            std::env::set_var("ICICLE_BACKEND", backend);
        }
        info!(target: "reth::icicle", backend = backend, "Icicle backend selected");
    }
    if let Some(device) = config.device {
        // SAFETY: See above for process-global env var configuration.
        unsafe {
            std::env::set_var("ICICLE_DEVICE", device.to_string());
        }
        info!(target: "reth::icicle", device, "Icicle device selected");
    }
    if let Some(dir) = &config.backend_dir {
        // SAFETY: See above for process-global env var configuration.
        unsafe {
            std::env::set_var("ICICLE_BACKEND_INSTALL_DIR", dir.to_string_lossy().as_ref());
        }
    }

    if std::env::var_os("ICICLE_LICENSE").is_none() {
        warn!(target: "reth::icicle", "ICICLE_LICENSE is not set; Icicle backend may fail to load");
    }

    icicle_runtime::runtime::load_backend_from_env_or_default()
        .map_err(|err| IcicleError(format!("{err:?}")))?;

    Ok(())
}

#[cfg(not(feature = "icicle"))]
fn init_runtime(_config: &IcicleConfig) -> Result<(), IcicleError> {
    Err(IcicleError("icicle feature not enabled".to_string()))
}

#[cfg(feature = "icicle")]
fn keccak256_batch_fixed_gpu(inputs: &[u8], item_len: usize) -> Result<Vec<B256>, IcicleError> {
    use icicle_core::hash::HashConfig;
    use icicle_hash::keccak::Keccak256;
    use icicle_runtime::memory::HostSlice;

    let batch = inputs.len() / item_len;
    if batch == 0 {
        return Ok(Vec::new());
    }

    let mut output = vec![0u8; batch * 32];
    let config = HashConfig::default();

    let hasher = Keccak256::new(0).map_err(|err| IcicleError(format!("{err:?}")))?;
    let started = Instant::now();
    hasher
        .hash(
            HostSlice::from_slice(inputs),
            &config,
            HostSlice::from_mut_slice(&mut output),
        )
        .map_err(|err| IcicleError(format!("{err:?}")))?;
    record_icicle_call(IcicleCallKind::Hash, started.elapsed());

    Ok(output.chunks_exact(32).map(B256::from_slice).collect())
}

#[cfg(not(feature = "icicle"))]
fn keccak256_batch_fixed_gpu(_inputs: &[u8], _item_len: usize) -> Result<Vec<B256>, IcicleError> {
    Err(IcicleError("icicle feature not enabled".to_string()))
}

#[cfg(feature = "icicle")]
mod bn254 {
    use super::{Bn254Error, IcicleCallKind, record_icicle_call};
    use ark_bn254::{Fq, Fq2, G1Affine as ArkG1Affine, G2Affine as ArkG2Affine};
    use ark_ec::AffineRepr;
    use ark_ff::{BigInteger, PrimeField, Zero};
    use ark_serialize::CanonicalDeserialize;
    use icicle_bn254::curve::{
        BaseField, G1Affine, G1Projective, G2Affine, G2BaseField, G2Projective, ScalarField,
    };
    use icicle_bn254::pairing::PairingTargetField;
    use icicle_core::{affine::Affine, bignum::BigNum};
    use icicle_core::pairing::pairing as icicle_pairing;
    use std::time::Instant;

    const FQ_LEN: usize = 32;
    const SCALAR_LEN: usize = 32;
    const FQ2_LEN: usize = 2 * FQ_LEN;
    const G1_LEN: usize = 2 * FQ_LEN;

    #[inline]
    fn read_fq(input_be: &[u8]) -> Result<Fq, Bn254Error> {
        if input_be.len() != FQ_LEN {
            return Err(Bn254Error::InvalidFieldElement);
        }

        let mut input_le = [0u8; FQ_LEN];
        input_le.copy_from_slice(input_be);
        input_le.reverse();

        Fq::deserialize_uncompressed(&input_le[..]).map_err(|_| Bn254Error::InvalidFieldElement)
    }

    #[inline]
    fn read_fq2(input: &[u8]) -> Result<Fq2, Bn254Error> {
        if input.len() != FQ2_LEN {
            return Err(Bn254Error::InvalidFieldElement);
        }
        let y = read_fq(&input[..FQ_LEN])?;
        let x = read_fq(&input[FQ_LEN..2 * FQ_LEN])?;
        Ok(Fq2::new(x, y))
    }

    #[inline]
    fn new_g1_point(px: Fq, py: Fq) -> Result<ArkG1Affine, Bn254Error> {
        if px.is_zero() && py.is_zero() {
            Ok(ArkG1Affine::zero())
        } else {
            let point = ArkG1Affine::new_unchecked(px, py);
            if !point.is_on_curve() || !point.is_in_correct_subgroup_assuming_on_curve() {
                return Err(Bn254Error::InvalidPoint);
            }
            Ok(point)
        }
    }

    #[inline]
    fn new_g2_point(x: Fq2, y: Fq2) -> Result<ArkG2Affine, Bn254Error> {
        let point = if x.is_zero() && y.is_zero() {
            ArkG2Affine::zero()
        } else {
            let point = ArkG2Affine::new_unchecked(x, y);
            if !point.is_on_curve() || !point.is_in_correct_subgroup_assuming_on_curve() {
                return Err(Bn254Error::InvalidPoint);
            }
            point
        };
        Ok(point)
    }

    #[inline]
    fn read_g1_point(input: &[u8]) -> Result<ArkG1Affine, Bn254Error> {
        if input.len() != G1_LEN {
            return Err(Bn254Error::InvalidFieldElement);
        }
        let px = read_fq(&input[0..FQ_LEN])?;
        let py = read_fq(&input[FQ_LEN..2 * FQ_LEN])?;
        new_g1_point(px, py)
    }

    #[inline]
    fn read_g2_point(input: &[u8]) -> Result<ArkG2Affine, Bn254Error> {
        if input.len() != 2 * FQ2_LEN {
            return Err(Bn254Error::InvalidFieldElement);
        }
        let ba = read_fq2(&input[0..FQ2_LEN])?;
        let bb = read_fq2(&input[FQ2_LEN..2 * FQ2_LEN])?;
        new_g2_point(ba, bb)
    }

    #[inline]
    fn read_scalar(input_be: &[u8]) -> ScalarField {
        let mut input_le = [0u8; SCALAR_LEN];
        let len = input_be.len().min(SCALAR_LEN);
        input_le[..len].copy_from_slice(&input_be[..len]);
        input_le.reverse();
        ScalarField::from_bytes_le(&input_le)
    }

    #[inline]
    fn fq_to_bytes_le(fq: &Fq) -> [u8; FQ_LEN] {
        let mut bytes = fq.into_bigint().to_bytes_le();
        bytes.resize(FQ_LEN, 0u8);
        let mut out = [0u8; FQ_LEN];
        out.copy_from_slice(&bytes[..FQ_LEN]);
        out
    }

    #[inline]
    fn fq2_to_bytes_le(fq2: &Fq2) -> [u8; FQ2_LEN] {
        let mut out = [0u8; FQ2_LEN];
        out[..FQ_LEN].copy_from_slice(&fq_to_bytes_le(&fq2.c0));
        out[FQ_LEN..].copy_from_slice(&fq_to_bytes_le(&fq2.c1));
        out
    }

    #[inline]
    fn ark_g1_to_icicle(point: &ArkG1Affine) -> G1Affine {
        if point.is_zero() {
            return G1Affine::zero();
        }

        let x = BaseField::from_bytes_le(&fq_to_bytes_le(&point.x));
        let y = BaseField::from_bytes_le(&fq_to_bytes_le(&point.y));
        G1Affine::from_xy(x, y)
    }

    #[inline]
    fn ark_g2_to_icicle(point: &ArkG2Affine) -> G2Affine {
        if point.is_zero() {
            return G2Affine::zero();
        }

        let x = G2BaseField::from_bytes_le(&fq2_to_bytes_le(&point.x));
        let y = G2BaseField::from_bytes_le(&fq2_to_bytes_le(&point.y));
        G2Affine::from_xy(x, y)
    }

    #[inline]
    fn field_to_be_32(field: BaseField) -> [u8; FQ_LEN] {
        let mut bytes = field.to_bytes_le();
        bytes.resize(FQ_LEN, 0u8);
        let mut out = [0u8; FQ_LEN];
        out.copy_from_slice(&bytes[..FQ_LEN]);
        out.reverse();
        out
    }

    #[inline]
    fn encode_g1_point(point: G1Affine) -> [u8; G1_LEN] {
        if point == G1Affine::zero() {
            return [0u8; G1_LEN];
        }

        let mut output = [0u8; G1_LEN];
        let x_bytes = field_to_be_32(point.x);
        let y_bytes = field_to_be_32(point.y);
        output[..FQ_LEN].copy_from_slice(&x_bytes);
        output[FQ_LEN..].copy_from_slice(&y_bytes);
        output
    }

    pub(super) fn g1_add(p1_bytes: &[u8], p2_bytes: &[u8]) -> Result<[u8; G1_LEN], Bn254Error> {
        let p1 = read_g1_point(p1_bytes)?;
        let p2 = read_g1_point(p2_bytes)?;

        let p1_icicle = ark_g1_to_icicle(&p1);
        let p2_icicle = ark_g1_to_icicle(&p2);

        let started = Instant::now();
        let result = G1Projective::from(p1_icicle) + G1Projective::from(p2_icicle);
        record_icicle_call(IcicleCallKind::Bn254, started.elapsed());
        Ok(encode_g1_point(result.into()))
    }

    pub(super) fn g1_mul(point_bytes: &[u8], scalar_bytes: &[u8]) -> Result<[u8; G1_LEN], Bn254Error> {
        let point = read_g1_point(point_bytes)?;
        let scalar = read_scalar(scalar_bytes);

        let point_icicle = ark_g1_to_icicle(&point);
        let started = Instant::now();
        let result = G1Projective::from(point_icicle) * scalar;
        record_icicle_call(IcicleCallKind::Bn254, started.elapsed());
        Ok(encode_g1_point(result.into()))
    }

    pub(super) fn pairing_check(pairs: &[(&[u8], &[u8])]) -> Result<bool, Bn254Error> {
        let mut acc = PairingTargetField::one();
        let mut any = false;

        for (g1_bytes, g2_bytes) in pairs {
            let g1 = read_g1_point(g1_bytes)?;
            let g2 = read_g2_point(g2_bytes)?;

            if g1.is_zero() || g2.is_zero() {
                continue;
            }
            any = true;

            let g1_icicle = ark_g1_to_icicle(&g1);
            let g2_icicle = ark_g2_to_icicle(&g2);

            let started = Instant::now();
            let gt = icicle_pairing::<G1Projective, G2Projective, PairingTargetField>(
                &g1_icicle,
                &g2_icicle,
            )
            .map_err(|err| Bn254Error::Backend(format!("{err:?}")))?;
            record_icicle_call(IcicleCallKind::Bn254, started.elapsed());

            acc = acc * gt;
        }

        if !any {
            return Ok(true);
        }

        Ok(acc == PairingTargetField::one())
    }
}

/// BN254 G1 addition using Icicle (EVM precompile compatible).
#[cfg(feature = "icicle")]
pub fn bn254_g1_add(p1_bytes: &[u8], p2_bytes: &[u8]) -> Result<[u8; 64], Bn254Error> {
    bn254::g1_add(p1_bytes, p2_bytes)
}

/// BN254 G1 scalar multiplication using Icicle (EVM precompile compatible).
#[cfg(feature = "icicle")]
pub fn bn254_g1_mul(point_bytes: &[u8], scalar_bytes: &[u8]) -> Result<[u8; 64], Bn254Error> {
    bn254::g1_mul(point_bytes, scalar_bytes)
}

/// BN254 pairing check using Icicle (EVM precompile compatible).
#[cfg(feature = "icicle")]
pub fn bn254_pairing_check(pairs: &[(&[u8], &[u8])]) -> Result<bool, Bn254Error> {
    bn254::pairing_check(pairs)
}

#[cfg(not(feature = "icicle"))]
pub fn bn254_g1_add(_p1_bytes: &[u8], _p2_bytes: &[u8]) -> Result<[u8; 64], Bn254Error> {
    Err(Bn254Error::Backend("icicle feature not enabled".to_string()))
}

#[cfg(not(feature = "icicle"))]
pub fn bn254_g1_mul(_point_bytes: &[u8], _scalar_bytes: &[u8]) -> Result<[u8; 64], Bn254Error> {
    Err(Bn254Error::Backend("icicle feature not enabled".to_string()))
}

#[cfg(not(feature = "icicle"))]
pub fn bn254_pairing_check(_pairs: &[(&[u8], &[u8])]) -> Result<bool, Bn254Error> {
    Err(Bn254Error::Backend("icicle feature not enabled".to_string()))
}

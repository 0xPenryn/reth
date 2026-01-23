//! Icicle GPU acceleration helpers.

use alloy_primitives::{keccak256, B256};
use reth_config::config::{IcicleBackend, IcicleConfig};
use reth_trie_common::{HashedPostState, HashedStorage, KeccakKeyHasher};
use revm_database::BundleState;
use std::sync::OnceLock;
use tracing::{debug, warn};

/// Icicle helper error wrapper.
#[derive(Debug, thiserror::Error)]
#[error("icicle error: {0}")]
pub struct IcicleError(String);

#[derive(Debug, Clone)]
struct IcicleState {
    config: IcicleConfig,
    available: bool,
}

static ICICLE_STATE: OnceLock<IcicleState> = OnceLock::new();

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
    }
    if let Some(device) = config.device {
        // SAFETY: See above for process-global env var configuration.
        unsafe {
            std::env::set_var("ICICLE_DEVICE", device.to_string());
        }
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
    hasher
        .hash(
            HostSlice::from_slice(inputs),
            &config,
            HostSlice::from_mut_slice(&mut output),
        )
        .map_err(|err| IcicleError(format!("{err:?}")))?;

    Ok(output.chunks_exact(32).map(B256::from_slice).collect())
}

#[cfg(not(feature = "icicle"))]
fn keccak256_batch_fixed_gpu(_inputs: &[u8], _item_len: usize) -> Result<Vec<B256>, IcicleError> {
    Err(IcicleError("icicle feature not enabled".to_string()))
}

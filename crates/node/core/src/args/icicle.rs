use clap::{Args, ValueEnum};
use reth_config::config::{IcicleBackend, IcicleConfig};
use std::path::PathBuf;

/// CLI arguments for Icicle GPU acceleration.
#[derive(Debug, Clone, Args, Default)]
pub struct IcicleArgs {
    /// Enable Icicle GPU acceleration (requires `--features icicle`).
    #[arg(long = "icicle")]
    pub enabled: bool,

    /// Backend selection for Icicle runtime.
    #[arg(long = "icicle-backend", value_enum)]
    pub backend: Option<IcicleBackendArg>,

    /// Optional backend install directory override.
    #[arg(long = "icicle-backend-dir", value_name = "PATH")]
    pub backend_dir: Option<PathBuf>,

    /// GPU device index to select.
    #[arg(long = "icicle-device", value_name = "INDEX")]
    pub device: Option<u32>,

    /// Minimum batch size for Keccak hashing offload.
    #[arg(long = "icicle-min-batch", value_name = "N")]
    pub min_batch: Option<usize>,

    /// Disable hashing stage acceleration.
    #[arg(long = "icicle-disable-hashing")]
    pub disable_hashing: bool,

    /// Disable state root / trie acceleration.
    #[arg(long = "icicle-disable-state-root")]
    pub disable_state_root: bool,

    /// Disable precompile acceleration.
    #[arg(long = "icicle-disable-precompiles")]
    pub disable_precompiles: bool,

    /// Disable sender recovery acceleration.
    #[arg(long = "icicle-disable-sender-recovery")]
    pub disable_sender_recovery: bool,

    /// Enable KZG acceleration when supported.
    #[arg(long = "icicle-enable-kzg")]
    pub enable_kzg: bool,
}

impl IcicleArgs {
    /// Apply CLI overrides to the provided config.
    pub fn apply_to(&self, config: &mut IcicleConfig) {
        let any_override = self.backend.is_some()
            || self.backend_dir.is_some()
            || self.device.is_some()
            || self.min_batch.is_some()
            || self.disable_hashing
            || self.disable_state_root
            || self.disable_precompiles
            || self.disable_sender_recovery
            || self.enable_kzg;

        if self.enabled || any_override {
            config.enabled = true;
        }

        if let Some(backend) = self.backend {
            config.backend = backend.into();
        }
        if let Some(path) = self.backend_dir.clone() {
            config.backend_dir = Some(path);
        }
        if let Some(device) = self.device {
            config.device = Some(device);
        }
        if let Some(min_batch) = self.min_batch {
            config.min_batch = min_batch;
        }
        if self.disable_hashing {
            config.hashing = false;
        }
        if self.disable_state_root {
            config.state_root = false;
        }
        if self.disable_precompiles {
            config.precompiles = false;
        }
        if self.disable_sender_recovery {
            config.sender_recovery = false;
        }
        if self.enable_kzg {
            config.kzg = true;
        }
    }
}

/// Backend selection for CLI.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum IcicleBackendArg {
    Auto,
    Cuda,
    Metal,
}

impl From<IcicleBackendArg> for IcicleBackend {
    fn from(value: IcicleBackendArg) -> Self {
        match value {
            IcicleBackendArg::Auto => IcicleBackend::Auto,
            IcicleBackendArg::Cuda => IcicleBackend::Cuda,
            IcicleBackendArg::Metal => IcicleBackend::Metal,
        }
    }
}

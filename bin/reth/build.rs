//! Build script for `reth`.
//!
//! When `revmc` is enabled we must export revmc builtin symbols so JIT-produced
//! shared objects can resolve them at runtime.

fn main() {
    if std::env::var_os("CARGO_FEATURE_REVMC").is_some() {
        revmc_build::emit();
    }
}

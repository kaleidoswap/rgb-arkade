//! Filesystem layout. Everything lives under the project root, which is `$RGB_ARKADE_DIR` if set,
//! otherwise the repository root (the parent of `demo/`, fixed at compile time).
use std::path::PathBuf;

pub fn root() -> PathBuf {
    std::env::var_os("RGB_ARKADE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."))
}

/// Per-run state directories (wallets, consignments, proofs, summary.json).
pub fn state() -> PathBuf {
    root().join("state")
}

/// The arkade-regtest checkout used to drive the regtest stack.
pub fn regtest_dir() -> PathBuf {
    root().join("arkade-regtest")
}

/// Watchtower daemon data directories.
pub fn watchtowerd() -> PathBuf {
    root().join("watchtowerd")
}

/// Host-built helper binaries (e.g. `tools/arkdb`).
pub fn tools() -> PathBuf {
    root().join("tools")
}

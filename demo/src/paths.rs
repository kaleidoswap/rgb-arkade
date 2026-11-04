//! Filesystem layout and service endpoints. Everything lives under the project root, which is `$RGB_ARKADE_DIR` if set,
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

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

/// arkd gRPC/REST endpoint: `$RGB_ARKADE_ARKD_URL`, default the regtest stack's `http://localhost:7070`.
pub fn arkd_url() -> String {
    env_or("RGB_ARKADE_ARKD_URL", "http://localhost:7070")
}

/// Esplora API: `$RGB_ARKADE_ESPLORA_URL`, default `http://localhost:3000/api`.
pub fn esplora_url() -> String {
    env_or("RGB_ARKADE_ESPLORA_URL", "http://localhost:3000/api")
}

/// bitcoind RPC: `$RGB_ARKADE_BITCOIND_URL`, default `http://localhost:18443`.
pub fn bitcoind_url() -> String {
    env_or("RGB_ARKADE_BITCOIND_URL", "http://localhost:18443")
}

/// Electrum server for rgb-lib: `$RGB_ARKADE_ELECTRUM_URL`, default `tcp://localhost:50001`.
pub fn electrum_url() -> String {
    env_or("RGB_ARKADE_ELECTRUM_URL", "tcp://localhost:50001")
}

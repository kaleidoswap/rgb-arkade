//! `ark-rgb-demo`: RGB assets on Arkade, regtest scenarios and tools.
//!
//! Usage:
//!   ark-rgb-demo scenario <name> [args] [--offline-proof]
//!   ark-rgb-demo watchtowerd <dir>
//!   ark-rgb-demo bench <dir> <phase> <n> [--pause]
//!   ark-rgb-demo list
mod app;
mod basic;
mod batch_scenarios;
mod bench;
mod checkpoint_offline;
mod checkpoint_renew;
mod checkpoint_swap;
mod common;
mod cosign;
mod delegate;
mod delegate_renewal;
mod delegated;
mod emu_explore;
mod exit_watchtower;
mod forfeit;
mod long_offline;
mod offchain_transfer;
mod opret_probe;
mod paths;
mod proof;
mod regtest;
mod sweep_recovery;
mod stock_lane;
mod swap;
mod swap_ni;
mod swap_emu;
mod watchtowerd;
mod wire;

pub(crate) use common::*;
use std::path::PathBuf;

/// (name, args, description)
const SCENARIOS: &[(&str, &str, &str)] = &[
    ("basic", "", "asset issued on a VTXO's predicted checkpoint, 1 off-chain hop, unilateral exit, on-chain send"),
    ("basic-onchain-genesis", "", "same, with on-chain issuance and a jump-in tx"),
    ("offchain-transfer", "", "jump-in to a VTXO, 2 off-chain hops with asset change, unilateral exit, on-chain send"),
    ("refresh", "", "refresh of a colored VTXO (RGB commitment in the forfeit), hop, exit, on-chain send"),
    ("collab-exit", "", "collaborative exit of a colored VTXO to an on-chain address, on-chain send"),
    ("forfeit-negatives", "", "malformed checkpoint/forfeit commitments and unattested forfeits are rejected"),
    ("exit-watchtower", "", "keyless exit watchtower takes a colored VTXO on-chain before expiry (in-process)"),
    ("delegate-renewal-setup", "<dir>", "delegate renewal (O2+O1): owners hand a key-less delegate their colored VTXOs; negatives"),
    ("delegate-renewal-return", "<olivia|rita> <dir>", "delegate renewal: an owner returns after ~60 days (off-chain, or after a tower exit)"),
    ("delegate-renewal-report", "<dir>", "delegate renewal: per-renewal proof size and verification times"),
    ("long-offline-setup", "<dir>", "long offline: users board and register with watchtowerd"),
    ("long-offline-return", "<user> <dir>", "long offline: a user comes back (refresh, watchtower exit, or lost)"),
    ("sweep-recovery-setup", "<dir>", "sweep recovery: Alice gets a colored VTXO and goes offline"),
    ("sweep-recovery-return", "<dir>", "sweep recovery: after the sweep Alice recovers BTC + asset into a new VTXO"),
    ("verify-recovery", "<sweep-recovery dir>", "re-verify a recovery proof from chain data; a non-swept VTXO is rejected"),
    ("multi-user", "", "one batch, several users, two assets, BTC-only VTXOs, off-chain send with BTC + asset change"),
    ("failed-batch", "", "a colored batch fails, the asset is rolled back and the retry succeeds"),
    ("opret-probe", "", "which OP_RETURN outputs (script shape, value) this arkd takes in an Arkade tx; no RGB"),
    ("stock-lane", "", "on STOCK arkd: jump-in to a predicted checkpoint, swap with the commitment in the ark tx only, limits recorded"),
    ("checkpoint-renew", "", "arkd with only the checkpoint data OP_RETURN: renew an asset onto a fresher VTXO and send it, standard RGB"),
    ("checkpoint-swap", "", "arkd with only the checkpoint data OP_RETURN: atomic swap onto existing VTXOs, then a send to a receiver with no VTXO"),
    ("checkpoint-offline", "", "arkd with only the checkpoint data OP_RETURN: delegated renewal, keyless watchtower exit, on-chain send after exit"),
    ("swap-ni", "", "arkd with only the checkpoint data OP_RETURN: NON-interactive sale (maker presigns SINGLE|ACP, taker fills alone), change spent, renewals"),
    ("swap-ni-grief", "<exit|burn|expire>", "swap-ni offer, a taker submits the fill and never finalizes: lock, maker finalize, then exit / taker burn / batch expiry"),
    ("swap-emu", "[crash]", "arkd cponly + emulator:rgb: fixed-price sale enforced by an Arkade-Script covenant (fill/cancel/exit), negatives, cancel, renewals; crash = emulator dies after arkd submit"),
    ("emu-lots", "", "emulator outcome covenant: offer in fixed lots (Merkle of precommitted outcomes), recursive change offer, emulator-clock expiry + maker reclaim"),
    ("emu-escrow", "", "emulator outcome covenant: RGB escrow with arbiter (CSFS), 2-of-3 style outcomes, timeout refund"),
    ("emu-lending", "", "emulator outcome covenant: RGB collateral, repay (BTC to lender checked), oracle liquidation (CSFS price), default after maturity"),
    ("emu-vault", "", "emulator outcome covenant: RGB vault, hot unvault per time slot into a pinned pending VTXO, delayed withdraw to a precommitted destination, cold clawback"),
    ("emu-asset-swap", "", "emulator outcome covenant: RGB asset sold for an Arkade asset, the Arkade leg checked by asset-packet introspection"),
    ("swap", "", "atomic RGB-asset-for-bitcoin swap in one Arkade tx; refusal before signing rolls back"),
    ("multihop-exit", "", "app flows: 2 off-chain hops, collaborative exit of the change, unilateral exit + on-chain send"),
    ("fraud", "", "colluding operator co-signs two forfeits of one VTXO: fraud proof + online detection"),
];

fn usage() -> anyhow::Result<()> {
    println!("usage: ark-rgb-demo scenario <name> [args] [--offline-proof] | cli [dir] | app [port] | watchtowerd <dir> | delegated <dir> | proxy <listen> <target> | bench <dir> <phase> <n> [--pause] | list\n");
    println!("scenarios:");
    for (n, a, d) in SCENARIOS {
        println!("  {:<24} {:<22} {d}", n, a);
    }
    println!("\n--offline-proof: receivers validate with arkd paused, from the consignment + Ark proof + chain");
    println!("  (supported by basic*, offchain-transfer, refresh, collab-exit)");
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();

    // flags (`--offline-proof`, `--pause`) are read where they are used; positional args only here
    let args: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    let arg = |i: usize| -> anyhow::Result<String> {
        args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("missing argument {i}; run `ark-rgb-demo list`"))
    };
    match args.first().map(String::as_str) {
        Some("scenario") => match arg(1)?.as_str() {
            "basic" => basic::run(false).await,
            "basic-onchain-genesis" => basic::run(true).await,
            "offchain-transfer" => offchain_transfer::run().await,
            "refresh" => forfeit::run_refresh().await,
            "collab-exit" => forfeit::run_exit().await,
            "forfeit-negatives" => forfeit::run_neg().await,
            "exit-watchtower" => exit_watchtower::run_watchtower().await,
            "long-offline-setup" => long_offline::setup(PathBuf::from(arg(2)?)).await,
            "long-offline-return" => long_offline::return_user(&arg(2)?, PathBuf::from(arg(3)?)).await,
            "sweep-recovery-setup" => sweep_recovery::setup(PathBuf::from(arg(2)?)).await,
            "sweep-recovery-return" => sweep_recovery::return_user(PathBuf::from(arg(2)?)).await,
            "verify-recovery" => batch_scenarios::verify_recovery(PathBuf::from(arg(2)?)).await,
            "multi-user" => batch_scenarios::run_multi().await,
            "failed-batch" => batch_scenarios::run_fail().await,
            "fraud" => batch_scenarios::run_fraud().await,
            "multihop-exit" => app::run_multihop_exit().await,
            "swap" => swap::run().await,
            "checkpoint-renew" => checkpoint_renew::run().await,
            "checkpoint-swap" => checkpoint_swap::run().await,
            "swap-ni" => swap_ni::run(None).await,
            "swap-ni-grief" => swap_ni::run(Some(args.get(2).cloned().unwrap_or_else(|| "exit".into()))).await,
            "emu-lots" => emu_explore::run_lots().await,
            "emu-escrow" => emu_explore::run_escrow().await,
            "emu-lending" => emu_explore::run_lending().await,
            "emu-vault" => emu_explore::run_vault().await,
            "emu-asset-swap" => emu_explore::run_asset_swap().await,
            "emu-exit" => emu_explore::run_exit().await,
            "emu-renew" => emu_explore::run_renew().await,
            "emu-renew-batch" => emu_explore::run_renew_batch().await,
            "swap-emu" => swap_emu::run(args.get(2).cloned()).await,
            "checkpoint-offline" => checkpoint_offline::run().await,
            "delegate-renewal-setup" => delegate_renewal::setup(PathBuf::from(arg(2)?)).await,
            "delegate-renewal-return" => delegate_renewal::return_user(&arg(2)?, PathBuf::from(arg(3)?)).await,
            "delegate-renewal-report" => delegate_renewal::report(PathBuf::from(arg(2)?)).await,
            "threshold-delegate-setup" | "threshold-delegate-return" | "threshold-delegate-report" => {
                delegate_renewal::THRESHOLD.store(true, std::sync::atomic::Ordering::SeqCst);
                match arg(1)?.as_str() {
                    "threshold-delegate-setup" => delegate_renewal::setup(PathBuf::from(arg(2)?)).await,
                    "threshold-delegate-return" => delegate_renewal::return_user(&arg(2)?, PathBuf::from(arg(3)?)).await,
                    _ => delegate_renewal::report(PathBuf::from(arg(2)?)).await,
                }
            }
            "stock-lane" => stock_lane::run().await,
            "opret-probe" => opret_probe::run().await,
            other => {
                usage()?;
                anyhow::bail!("unknown scenario {other}")
            }
        },
        Some("cli") => app::run_cli(args.get(1).map(PathBuf::from)).await,
        Some("app") => app::run(args.get(1).map(|p| p.parse()).transpose()?.unwrap_or(8080)).await,
        Some("delegated") => delegated::run_daemon(PathBuf::from(arg(1)?)).await,
        Some("cosignd") => cosign::run_daemon(PathBuf::from(arg(1)?)).await,
        Some("proxy") => delegated::run_proxy(arg(1)?, arg(2)?).await,
        Some("watchtowerd") => watchtowerd::run_daemon(PathBuf::from(arg(1)?)).await,
        Some("bench") => bench::run(PathBuf::from(arg(1)?), &arg(2)?, arg(3)?.parse()?).await,
        _ => usage(),
    }
}

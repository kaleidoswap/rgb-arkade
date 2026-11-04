//! `ark-rgb-demo app [port]`: prototype web app (regtest). Two wallets (Alice, Bob) side by side;
//! every RGB-on-Arkade flow is one button / one `POST /api/action`. Actions run one at a time in
//! the background; `GET /api/state` returns balances, VTXOs, RGB holdings and the activity log.
use crate::batch_scenarios::{forfeit_proof, own};
use crate::forfeit::{BatchOp, attest_and_register, colored_batch, fund_onchain, mine_until, spend_exited, unilateral_exit};
use crate::long_offline::{pay_from_rgb_output, vtxo_info};
use crate::offchain_transfer::{rgb_hop, wait_vtxo};
use crate::proof::{add_vtxos, arkd_down, build_proof, import, mark_recovery, negatives, operators, proof_path};
use crate::{ArkClient, ark_client, ark_client_load, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, rgb_wallet_load};
use anyhow::{anyhow, bail, ensure};
use ark_bdk_wallet::Wallet as ArkBdkWallet;
use ark_client::GetVtxosRequestFilter;
use ark_core::send::SendReceiver;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use bitcoin::{Address, Amount, Network, OutPoint, Txid};
use regtest::Regtest;
use rgb_lib::wallet::{Online, RgbWalletOpsOffline, RgbWalletOpsOnline, Wallet as RgbWallet};
use rgb_lib::{Assignment, ContractId, FileContent};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex as StdMutex};

const INDEX: &str = include_str!("../app/index.html");
const NAMES: [&str; 2] = ["alice", "bob"];
const EXIT_SAT: u64 = 20_000;
/// `ark-rgb-demo cli`: log lines are printed for a terminal instead of the app's server log
static CLI: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// In the CLI the flows' own output goes to `<dir>/cli.log`; the terminal (this fd) gets the steps.
static CLI_TTY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

fn cli_print(s: &str) {
    let fd = CLI_TTY.load(Ordering::Relaxed);
    // SAFETY: fd is stdout or its dup, open for the whole process; ManuallyDrop keeps it open
    let mut f = std::mem::ManuallyDrop::new(unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    let _ = std::io::Write::write_all(&mut *f, s.as_bytes());
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Holding {
    asset_id: String,
    amount: u64,
    /// vtxo | unrolled (VTXO exited on-chain, ark-client script) | exit-output (collaborative exit, rgb-lib address) | onchain
    kind: String,
    outpoint: String,
    /// offline Ark proof that comes with this holding (for the next transfer)
    proof: Option<String>,
}

struct Wallet {
    name: String,
    ark: ArkClient,
    ark_w: Arc<ArkBdkWallet>,
    rgb: RgbWallet,
    online: Online,
    holdings: Vec<Holding>,
    /// BTC-only outputs of this wallet put on-chain by a unilateral exit (siblings of the exited
    /// colored VTXO in its branch): claimable through the exit path, swept with `sweep_btc`
    exited_btc: Vec<String>,
}

/// What the app keeps on disk besides the wallets' own files (`<data dir>/app.json`).
#[derive(Serialize, Deserialize, Default)]
struct Saved {
    seq: u64,
    holdings: std::collections::BTreeMap<String, Vec<Holding>>,
    #[serde(default)]
    exited_btc: std::collections::BTreeMap<String, Vec<String>>,
    log: Vec<LogEntry>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct LogEntry {
    t: u64,
    action: String,
    kind: String,
    msg: String,
}

#[derive(Serialize, Clone, Default)]
struct Shared {
    busy: Option<String>,
    log: Vec<LogEntry>,
    wallets: Vec<serde_json::Value>,
    arkd: serde_json::Value,
    state_dir: String,
    height: u64,
    /// background miner interval in seconds (0 = off)
    automine: u64,
    /// public mode: one wallet pair per visitor, no global regtest controls
    public: bool,
    /// tx URL prefix of the block explorer ("" = the local mempool on :3000)
    explorer: String,
}

struct Core {
    state: PathBuf,
    regtest: Arc<Regtest>,
    ops: String,
    wallets: Vec<Wallet>,
    seq: u64,
}

/// One visitor's wallet pair, served by its own worker thread.
struct Session {
    jobs: StdMutex<std::sync::mpsc::Sender<Action>>,
    shared: Arc<StdMutex<Shared>>,
    last_seen: AtomicU64,
    ip: String,
}

#[derive(Clone)]
struct AppState {
    sessions: Arc<StdMutex<HashMap<String, Arc<Session>>>>,
    /// last session creation per client address (public mode)
    created: Arc<StdMutex<HashMap<String, u64>>>,
    public: bool,
    explorer: String,
}

/// Only one action runs at a time across all sessions: actions pause arkd and the electrum
/// indexer does not cope with concurrent scenario traffic.
static ACTION_LOCK: StdMutex<()> = StdMutex::new(());
/// Worker threads still alive, evicted ones included (each holds a runtime and wallets).
static LIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
const COOKIE: &str = "rgbarkade";
const LOCAL: &str = "local";
fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

#[derive(Deserialize)]
struct AutoMine {
    seconds: u64,
}

/// Regtest time control that runs beside the action queue: mine one block every `seconds`.
async fn post_automine(State(app): State<AppState>, Json(m): Json<AutoMine>) -> impl IntoResponse {
    if app.public {
        return Json(serde_json::json!({"ok": false, "error": "not available on the public demo"}));
    }
    let Some(sess) = app.sessions.lock().unwrap().get(LOCAL).cloned() else {
        return Json(serde_json::json!({"ok": false}));
    };
    let mut s = sess.shared.lock().unwrap();
    s.automine = m.seconds;
    let msg = if m.seconds == 0 { "auto-mine off".to_string() } else { format!("auto-mine: one block every {} s", m.seconds) };
    println!("[app] [automine] info: {msg}");
    s.log.push(LogEntry { t: now(), action: "automine".into(), kind: "info".into(), msg });
    Json(serde_json::json!({"ok": true}))
}

fn miner(shared: Arc<StdMutex<Shared>>) {
    let rt = Regtest::new();
    loop {
        let every = shared.lock().unwrap().automine;
        if every == 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        let _ = regtest::mine(1);
        if let Some(h) = rt.rpc("getblockcount", serde_json::json!([])).ok().and_then(|v| v.as_u64()) {
            shared.lock().unwrap().height = h;
        }
        std::thread::sleep(std::time::Duration::from_secs(every));
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    Board { who: String, sats: Option<u64> },
    Issue { who: String },
    // `outpoint` (optional) picks which colored output to use when a wallet holds several
    Send { from: String, to: String, asset_id: String, amount: u64, sats: Option<u64>, outpoint: Option<String> },
    /// `seller` gives `amount` units for `price` sat of `buyer`, in one Arkade tx (scenario `swap`)
    Swap { seller: String, buyer: String, asset_id: String, amount: u64, price: u64, outpoint: Option<String> },
    Refresh { who: String, asset_id: String, outpoint: Option<String> },
    CollabExit { who: String, asset_id: String, outpoint: Option<String> },
    UnilateralExit { who: String, asset_id: String, outpoint: Option<String> },
    OnchainSend { from: String, to: String, asset_id: String, outpoint: Option<String> },
    Recover { who: String, asset_id: String, outpoint: Option<String> },
    /// spend the BTC-only outputs a unilateral exit put on-chain to the wallet's on-chain address
    SweepBtc { who: String },
    Mine { blocks: Option<u32> },
    SetExpiry { seconds: u64 },
    Sync,
    /// delete the data dir and start with fresh wallets
    Reset,
}

impl Action {
    fn label(&self) -> String {
        match self {
            Action::Board { who, .. } => format!("{who}: board BTC"),
            Action::Issue { who } => format!("{who}: issue asset + jump-in"),
            Action::Send { from, to, amount, .. } => format!("{from} -> {to}: send {amount} off-chain"),
            Action::Swap { seller, buyer, amount, price, .. } => format!("{seller} -> {buyer}: swap {amount} for {price} sat"),
            Action::Refresh { who, .. } => format!("{who}: refresh in a batch"),
            Action::CollabExit { who, .. } => format!("{who}: collaborative exit"),
            Action::UnilateralExit { who, .. } => format!("{who}: unilateral exit"),
            Action::OnchainSend { from, to, .. } => format!("{from} -> {to}: on-chain send"),
            Action::Recover { who, .. } => format!("{who}: sweep recovery"),
            Action::SweepBtc { who } => format!("{who}: sweep exited BTC"),
            Action::Mine { blocks } => format!("mine {} block(s)", blocks.unwrap_or(1)),
            Action::SetExpiry { seconds } => format!("set batch expiry {seconds} {}", if *seconds < 512 { "blocks" } else { "s" }),
            Action::Sync => "sync".into(),
            Action::Reset => "reset".into(),
        }
    }
}

#[derive(Clone)]
struct Logger {
    shared: Arc<StdMutex<Shared>>,
    action: String,
}

impl Logger {
    fn push(&self, kind: &str, msg: impl Into<String>) {
        let msg = msg.into();
        if CLI.load(Ordering::Relaxed) {
            if !msg.starts_with("neg_") {
                cli_print(&format!("  {} {msg}\n", if kind == "tx" { "tx" } else { "··" }));
            }
        } else {
            println!("[app] [{}] {kind}: {msg}", self.action);
        }
        let mut s = self.shared.lock().unwrap();
        s.log.push(LogEntry { t: now(), action: self.action.clone(), kind: kind.into(), msg });
        let n = s.log.len();
        if n > 2000 {
            s.log.drain(0..n - 2000);
        }
    }
    fn info(&self, m: impl Into<String>) {
        self.push("info", m)
    }
    fn tx(&self, what: &str, txid: impl std::fmt::Display) {
        self.push("tx", format!("{what}: {txid}"))
    }
    fn rec(&self) -> impl FnMut(&str, String) + '_ {
        move |k: &str, v: String| self.push("info", format!("{k} = {v}"))
    }
}

fn idx(name: &str) -> anyhow::Result<usize> {
    NAMES.iter().position(|n| *n == name).ok_or_else(|| anyhow!("unknown wallet {name}"))
}

fn two_mut(ws: &mut [Wallet], a: usize, b: usize) -> anyhow::Result<(&mut Wallet, &mut Wallet)> {
    ensure!(a != b, "sender and receiver must differ");
    if a < b {
        let (l, r) = ws.split_at_mut(b);
        Ok((&mut l[a], &mut r[0]))
    } else {
        let (l, r) = ws.split_at_mut(a);
        Ok((&mut r[0], &mut l[b]))
    }
}

fn take_holding(w: &mut Wallet, asset_id: &str, kinds: &[&str], outpoint: &Option<String>) -> anyhow::Result<Holding> {
    let i = w
        .holdings
        .iter()
        .position(|h| h.asset_id == asset_id && kinds.contains(&h.kind.as_str()) && outpoint.as_ref().is_none_or(|o| *o == h.outpoint))
        .ok_or_else(|| anyhow!("{} holds no {asset_id} as {kinds:?}", w.name))?;
    Ok(w.holdings.remove(i))
}

fn blinding() -> u64 {
    (rand::random::<u32>() as u64) + 1
}

fn carried_witnesses(carried: &[PathBuf]) -> anyhow::Result<Vec<Txid>> {
    let mut ws = vec![];
    for c in carried {
        let f = crate::proof::load(&c)?;
        ws.extend(f.witnesses.iter().filter_map(|w| Txid::from_str(w).ok()));
    }
    Ok(ws)
}

fn set_expiry(seconds: u64) -> anyhow::Result<String> {
    let out = std::process::Command::new("curl")
        .args(["-s", "-X", "POST", "-H", "Content-Type: application/json", "http://localhost:7071/v1/admin/settings", "-d"])
        .arg(format!("{{\"settings\":{{\"vtxoTreeExpiry\":\"{seconds}\"}}}}"))
        .output()?;
    ensure!(out.status.success(), "admin API call failed");
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

async fn new_wallet(name: &str, regtest: &Arc<Regtest>, state: &Path) -> anyhow::Result<Wallet> {
    let (ark, ark_w) = ark_client(name, regtest.clone(), state).await?;
    let (rgb, online) = bl(|| rgb_wallet(name, state))?;
    Ok(Wallet { name: name.into(), ark, ark_w, rgb, online, holdings: vec![], exited_btc: vec![] })
}

/// Reopen a wallet from the data dir: same keys, rgb-lib data (stock + Ark registry), Ark
/// contracts rediscovered from arkd's indexer.
async fn load_wallet(name: &str, regtest: &Arc<Regtest>, state: &Path) -> anyhow::Result<Wallet> {
    let (ark, ark_w) = ark_client_load(name, regtest.clone(), state).await?;
    let (rgb, online) = bl(|| rgb_wallet_load(name, state))?;
    Ok(Wallet { name: name.into(), ark, ark_w, rgb, online, holdings: vec![], exited_btc: vec![] })
}

const SAVED: &str = "app.json";

fn save(c: &Core, shared: &Arc<StdMutex<Shared>>) -> anyhow::Result<()> {
    let saved = Saved {
        seq: c.seq,
        holdings: c.wallets.iter().map(|w| (w.name.clone(), w.holdings.clone())).collect(),
        exited_btc: c.wallets.iter().map(|w| (w.name.clone(), w.exited_btc.clone())).collect(),
        log: shared.lock().unwrap().log.clone(),
    };
    let tmp = c.state.join(format!("{SAVED}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&saved)?)?;
    std::fs::rename(tmp, c.state.join(SAVED))?;
    Ok(())
}

/// Outputs of the exit branch that pay one of `w`'s own VTXO scripts, other than `exited`.
/// Colored ones (live holdings) become `unrolled` holdings; BTC-only ones that are still live
/// VTXOs are kept for `sweep_btc`; ones already spent off-chain (forfeited in a batch, or spent by
/// an Arkade tx) belong to the operator / the next tx and are left alone.
async fn classify_unrolled(w: &mut Wallet, regtest: &Regtest, branch: &[Txid], exited: OutPoint, lg: &Logger) -> anyhow::Result<()> {
    let own: Vec<bitcoin::ScriptBuf> = w.ark.get_offchain_addresses().await?.iter().map(|(_, v)| v.script_pubkey()).collect();
    for t in branch {
        let Some(tx) = regtest.raw_tx(t) else { continue };
        for (vout, out) in tx.output.iter().enumerate() {
            let o = OutPoint { txid: *t, vout: vout as u32 };
            if o == exited || !own.contains(&out.script_pubkey) {
                continue;
            }
            if let Some(h) = w.holdings.iter_mut().find(|h| h.outpoint == o.to_string()) {
                if h.kind == "vtxo" {
                    h.kind = "unrolled".into();
                    lg.info(format!("{o} ({} of the asset) is in the same branch: now on-chain too (unrolled)", h.amount));
                }
                continue;
            }
            match vtxo_info(o).await {
                Some(i) if i.is_spent => lg.info(format!("{o} ({}) is in the branch but was already spent off-chain (forfeited / spent by an Arkade tx): not ours to claim, not swept", out.value)),
                _ => {
                    lg.info(format!("{o} ({}) is a BTC-only VTXO of {} in the branch: on-chain now, sweepable after the exit delay", out.value, w.name));
                    if !w.exited_btc.contains(&o.to_string()) {
                        w.exited_btc.push(o.to_string());
                    }
                }
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------- actions

async fn run_action(c: &mut Core, a: &Action, lg: &Logger) -> anyhow::Result<()> {
    let regtest = c.regtest.clone();
    let ops = c.ops.clone();
    c.seq += 1;
    let seq = c.seq;
    let state = c.state.clone();
    match a.clone() {
        Action::Board { who, sats } => {
            let w = &mut c.wallets[idx(&who)?];
            let btc = format!("{:.8}", sats.unwrap_or(100_000) as f64 / 1e8);
            lg.info(format!("faucet {btc} BTC to {who}'s boarding address, then settle into a batch"));
            board(&who, &w.ark, &btc).await?;
            let l = w.ark.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?;
            for v in l.spendable_offchain() {
                lg.info(format!("{who} VTXO {} {}", v.vtxo().outpoint, v.vtxo().amount));
            }
            lg.push("ok", format!("{who} boarded"));
        }
        Action::Issue { who } => {
            let w = &mut c.wallets[idx(&who)?];
            let colored: Vec<String> = w.holdings.iter().map(|h| h.outpoint.clone()).collect();
            let l = w.ark.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?;
            let v = l
                .spendable_offchain()
                .map(|v| v.vtxo().outpoint)
                .find(|o| !colored.contains(&o.to_string()))
                .ok_or_else(|| anyhow!("{who} has no BTC-only VTXO: board first"))?;
            lg.info(format!("issue NIA on-chain (stock rgb-lib), then jump-in transfer to {who}'s VTXO {v}"));
            let asset_id = {
                let mut rec = lg.rec();
                onchain_genesis_jump_in(&regtest, &mut w.rgb, &w.online, v, &state, &mut rec).await?
            };
            // track V in rgb-lib's Ark registry (branch from arkd, verified against the chain)
            let p = state.join(format!("{seq}-track-{who}.arkproof.pb"));
            add_vtxos(&w.ark, &regtest, &p, &[v]).await?;
            import(&w.rgb, &w.online, &p, &ops)?;
            own(&w.rgb, v)?;
            w.holdings.push(Holding { asset_id: asset_id.clone(), amount: crate::ISSUE_AMOUNT, kind: "vtxo".into(), outpoint: v.to_string(), proof: None });
            lg.push("ok", format!("{who} holds {} of {asset_id} on VTXO {v}", crate::ISSUE_AMOUNT));
        }
        Action::Send { from, to, asset_id, amount, sats, outpoint } => {
            let (fi, ti) = (idx(&from)?, idx(&to)?);
            let (f, t) = two_mut(&mut c.wallets, fi, ti)?;
            let h = take_holding(f, &asset_id, &["vtxo"], &outpoint)?;
            let res: anyhow::Result<()> = async {
                ensure!(amount > 0 && amount <= h.amount, "amount must be 1..={}", h.amount);
                let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
                let v = OutPoint::from_str(&h.outpoint)?;
                // the change VTXO must keep enough sats to be spent/settled again: split the VTXO's
                // sats in half on a partial send, move all of them on a full send
                let have = vtxo_info(v).await.ok_or_else(|| anyhow!("indexer: {v} unknown"))?.amount.to_sat();
                let sats = Amount::from_sat(match sats {
                    Some(s) => s,
                    None if amount == h.amount => have,
                    None => have / 2,
                });
                ensure!(sats.to_sat() >= 330 && (amount == h.amount || have - sats.to_sat() >= 330), "{v} has {have} sat: too little to split (refresh or send the whole amount)");
                let (to_addr, _) = t.ark.get_offchain_address().await?;
                let (chg, chg_v) = f.ark.get_offchain_address().await?;
                let (bt, bc, bcp) = (blinding(), blinding(), blinding());
                let change = h.amount - amount;
                let mut outs = vec![(1u32, amount, bt)];
                if change > 0 {
                    outs.push((2, change, bc));
                }
                let cons = state.join(format!("{seq}-{from}-to-{to}.rgbc"));
                lg.info(format!("T1 in the checkpoint tx, T2 in the Arkade tx: {amount} to {to} (vout 1){}", if change > 0 { format!(", {change} change to {from} (vout 2)") } else { String::new() }));
                let hop = rgb_hop(&format!("{from}->{to}"), &f.ark, &f.rgb, v, vec![SendReceiver::bitcoin(to_addr, sats)], &chg, chg_v.owner_pk(), cid, outs, bcp, cons.clone()).await?;
                lg.tx("checkpoint tx (T1)", hop.checkpoint_txid);
                lg.tx("Arkade tx (T2)", hop.ark_txid);
                let vt = OutPoint { txid: hop.ark_txid, vout: 1 };
                let vc = OutPoint { txid: hop.ark_txid, vout: 2 };
                wait_vtxo(&t.ark, vt, &to).await?;
                let carried: Vec<PathBuf> = h.proof.iter().map(PathBuf::from).collect();
                let mut ws = carried_witnesses(&carried)?;
                ws.extend([hop.checkpoint_txid, hop.ark_txid]);
                let p = proof_path(&cons);
                let st = build_proof(&f.ark, &regtest, &ws, &carried, &p).await?;
                let mut tracked = vec![vt];
                if change > 0 {
                    tracked.push(vc);
                }
                add_vtxos(&f.ark, &regtest, &p, &tracked).await?;
                lg.info(format!("offline Ark proof {}: {} B, {} PSBTs, {} unmined witnesses; consignment {} B", p.display(), crate::proof::file_len(&p), st.n_psbts, st.n_witnesses, crate::proof::file_len(&cons)));
                {
                    let mut rec = lg.rec();
                    let _down = arkd_down(&mut rec, &to)?;
                    lg.info(format!("arkd PAUSED: {to} validates from consignment + Ark proof + bitcoin chain only"));
                    let neg = negatives(&to, &t.rgb, &t.online, &p, &ops, &mut rec)?;
                    ensure!(neg, "a tampered proof was accepted");
                    lg.info("tampered proofs rejected (negative checks)");
                    let reg = import(&t.rgb, &t.online, &p, &ops)?;
                    lg.info(format!("{to} registered {} Ark witnesses", reg.len()));
                    let (_, assign) = bl(|| t.rgb.accept_transfer_consignment_offchain(t.online.clone(), cons.clone(), vec![], hop.ark_txid.to_string(), 1, bt))?;
                    lg.info(format!("{to} accepted: {assign:?}"));
                }
                lg.info("arkd unpaused");
                import(&f.rgb, &f.online, &p, &ops)?;
                let ps = Some(p.display().to_string());
                if change > 0 {
                    own(&f.rgb, vc)?;
                    f.holdings.push(Holding { asset_id: asset_id.clone(), amount: change, kind: "vtxo".into(), outpoint: vc.to_string(), proof: ps.clone() });
                }
                t.holdings.push(Holding { asset_id: asset_id.clone(), amount, kind: "vtxo".into(), outpoint: vt.to_string(), proof: ps });
                lg.push("ok", format!("{to} received {amount} off-chain on VTXO {vt}"));
                Ok(())
            }
            .await;
            if let Err(e) = res {
                if f.holdings.iter().all(|x| x.outpoint != h.outpoint) && !matches!(vtxo_info(OutPoint::from_str(&h.outpoint)?).await, Some(ref i) if i.is_spent) {
                    f.holdings.push(h);
                }
                return Err(e);
            }
        }
        Action::Swap { seller, buyer, asset_id, amount, price, outpoint } => {
            let (si, bi) = (idx(&seller)?, idx(&buyer)?);
            let (f, t) = two_mut(&mut c.wallets, si, bi)?;
            let h = take_holding(f, &asset_id, &["vtxo"], &outpoint)?;
            let res: anyhow::Result<()> = async {
                ensure!(amount > 0 && amount <= h.amount, "amount must be 1..={}", h.amount);
                let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
                let v = OutPoint::from_str(&h.outpoint)?;
                // the buyer pays from a bitcoin-only VTXO that covers the price
                let colored: Vec<String> = t.holdings.iter().map(|x| x.outpoint.clone()).collect();
                let bv = t
                    .ark
                    .list_vtxos_filtered(GetVtxosRequestFilter::Spendable)
                    .await?
                    .spendable_offchain()
                    .map(|x| x.vtxo().clone())
                    .filter(|x| !colored.contains(&x.outpoint.to_string()) && x.amount.to_sat() >= price)
                    .max_by_key(|x| x.amount)
                    .map(|x| x.outpoint)
                    .ok_or_else(|| anyhow!("{buyer} has no bitcoin-only VTXO worth {price} sat: add bitcoin first"))?;
                // an asset that already moved off-chain carries its Ark proof: the buyer imports it
                // first, so it can check the new transition on top of that history before signing
                let carried: Vec<PathBuf> = h.proof.iter().map(PathBuf::from).collect();
                for p in &carried {
                    import(&t.rgb, &t.online, p, &ops)?;
                }
                let terms = crate::swap::Terms {
                    contract_id: cid,
                    asset: amount,
                    price,
                    carrier: 1_000,
                    blinding_buyer: blinding(),
                    blinding_change: blinding(),
                    blinding_cp: blinding(),
                };
                let cons = state.join(format!("{seq}-{seller}-swap-{buyer}.rgbc"));
                lg.info(format!("one Arkade tx spends {seller}'s asset VTXO {v} and {buyer}'s bitcoin VTXO {bv}"));
                let sw = {
                    let s_side = crate::swap::Side { name: &seller, ark: &f.ark, rgb: &f.rgb, online: &f.online, vtxo: v };
                    let b_side = crate::swap::Side { name: &buyer, ark: &t.ark, rgb: &t.rgb, online: &t.online, vtxo: bv };
                    crate::swap::atomic_swap(&s_side, &b_side, &terms, cons.clone(), crate::swap::Case::Agreed).await?
                }
                .ok_or_else(|| anyhow!("the swap did not happen"))?;
                lg.info(format!("{buyer} checked {amount} units on the unsigned txs before signing, then signed its side up front; {seller} completed alone"));
                lg.tx("checkpoint tx (T1)", sw.seller_checkpoint);
                lg.tx("checkpoint tx (buyer)", sw.buyer_checkpoint);
                lg.tx("Arkade tx (T2)", sw.ark_txid);
                let vt = OutPoint { txid: sw.ark_txid, vout: 1 };
                let vc = OutPoint { txid: sw.ark_txid, vout: 2 };
                wait_vtxo(&t.ark, vt, &buyer).await?;
                let mut ws = carried_witnesses(&carried)?;
                ws.extend([sw.seller_checkpoint, sw.ark_txid]);
                let p = proof_path(&cons);
                build_proof(&f.ark, &regtest, &ws, &carried, &p).await?;
                let change = h.amount - amount;
                let mut tracked = vec![vt];
                if change > 0 {
                    tracked.push(vc);
                }
                add_vtxos(&f.ark, &regtest, &p, &tracked).await?;
                {
                    let mut rec = lg.rec();
                    let _down = arkd_down(&mut rec, &buyer)?;
                    lg.info(format!("arkd PAUSED: {buyer} validates from consignment + Ark proof + bitcoin chain only"));
                    let neg = negatives(&buyer, &t.rgb, &t.online, &p, &ops, &mut rec)?;
                    ensure!(neg, "a tampered proof was accepted");
                    let reg = import(&t.rgb, &t.online, &p, &ops)?;
                    lg.info(format!("{buyer} registered {} Ark witnesses", reg.len()));
                    let (_, assign) = bl(|| t.rgb.accept_transfer_consignment_offchain(t.online.clone(), cons.clone(), vec![], sw.ark_txid.to_string(), 1, terms.blinding_buyer))?;
                    lg.info(format!("{buyer} accepted: {assign:?}"));
                }
                lg.info("arkd unpaused");
                import(&f.rgb, &f.online, &p, &ops)?;
                let ps = Some(p.display().to_string());
                if change > 0 {
                    own(&f.rgb, vc)?;
                    f.holdings.push(Holding { asset_id: asset_id.clone(), amount: change, kind: "vtxo".into(), outpoint: vc.to_string(), proof: ps.clone() });
                }
                t.holdings.push(Holding { asset_id: asset_id.clone(), amount, kind: "vtxo".into(), outpoint: vt.to_string(), proof: ps });
                lg.push("ok", format!("{buyer} got {amount} units, {seller} got {price} sat, in Arkade tx {}", sw.ark_txid));
                Ok(())
            }
            .await;
            if let Err(e) = res {
                if f.holdings.iter().all(|x| x.outpoint != h.outpoint) && !matches!(vtxo_info(OutPoint::from_str(&h.outpoint)?).await, Some(ref i) if i.is_spent) {
                    f.holdings.push(h);
                }
                return Err(e);
            }
        }
        Action::Refresh { who, asset_id, outpoint } | Action::Recover { who, asset_id, outpoint } => {
            let recover = matches!(a, Action::Recover { .. });
            let w = &mut c.wallets[idx(&who)?];
            let mut outpoint = outpoint;
            if outpoint.is_none() {
                // refresh prefers a live VTXO, recover a swept one
                for h in w.holdings.iter().filter(|h| h.asset_id == asset_id && h.kind == "vtxo") {
                    let swept = match OutPoint::from_str(&h.outpoint) {
                        Ok(o) => vtxo_info(o).await.is_some_and(|i| i.is_swept),
                        Err(_) => false,
                    };
                    if swept == recover {
                        outpoint = Some(h.outpoint.clone());
                        break;
                    }
                }
            }
            let h = take_holding(w, &asset_id, &["vtxo"], &outpoint)?;
            let v = OutPoint::from_str(&h.outpoint)?;
            let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
            if recover {
                let vi = vtxo_info(v).await.ok_or_else(|| anyhow!("indexer"))?;
                if !vi.is_swept {
                    w.holdings.push(h);
                    bail!("{v} is not swept (expires_at {}); use refresh", vi.expires_at);
                }
                ark_core::intent::RGB_RECOVERY_FORFEIT.store(true, Ordering::SeqCst);
                lg.info(format!("{v} was swept by the operator: recover it with an RGB-carrying recovery forfeit"));
            }
            let (addr, _) = w.ark.get_offchain_address().await?;
            lg.info(format!("join the next batch; the forfeit tx of {v} commits T: {v} -> new VTXO"));
            let r = colored_batch(&who, &w.ark, &w.rgb, cid, v, h.amount, BatchOp::Refresh(addr)).await;
            ark_core::intent::RGB_RECOVERY_FORFEIT.store(false, Ordering::SeqCst);
            let (commit, fc) = match r {
                Ok(x) => x,
                Err(e) => {
                    w.holdings.push(h);
                    return Err(e);
                }
            };
            lg.tx("commitment tx", commit);
            lg.tx("forfeit tx (carries the RGB transition)", fc.forfeit_tx.compute_txid());
            mine_until(&regtest, &commit).await?;
            let carried: Vec<PathBuf> = h.proof.iter().map(PathBuf::from).collect();
            let cons = state.join(format!("{seq}-{who}-{}.rgbc", if recover { "recovery" } else { "refresh" }));
            let p = forfeit_proof(&w.ark, &regtest, &fc, &carried, &cons).await?;
            if recover {
                mark_recovery(&p, fc.forfeit_tx.compute_txid())?;
            }
            import(&w.rgb, &w.online, &p, &ops)?;
            own(&w.rgb, fc.target)?;
            w.holdings.push(Holding { asset_id, amount: h.amount, kind: "vtxo".into(), outpoint: fc.target.to_string(), proof: Some(p.display().to_string()) });
            lg.push("ok", format!("{who}: {} now on new VTXO {}", h.amount, fc.target));
        }
        Action::CollabExit { who, asset_id, outpoint } => {
            let w = &mut c.wallets[idx(&who)?];
            let h = take_holding(w, &asset_id, &["vtxo"], &outpoint)?;
            let v = OutPoint::from_str(&h.outpoint)?;
            let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
            let exit_addr = Address::from_str(&bl(|| w.rgb.get_address())?)?.assume_checked();
            // exit EXIT_SAT, or the whole VTXO when it holds less (e.g. change after a few sends)
            let exit_sat = vtxo_info(v).await.map(|i| i.amount.to_sat()).unwrap_or(EXIT_SAT).min(EXIT_SAT);
            lg.info(format!("collaborative exit of {v} to {exit_addr} ({exit_sat} sat); the forfeit commits T: {v} -> commitment-tx output"));
            let (commit, fc) = match colored_batch(&who, &w.ark, &w.rgb, cid, v, h.amount, BatchOp::Exit(exit_addr, Amount::from_sat(exit_sat))).await {
                Ok(x) => x,
                Err(e) => {
                    w.holdings.push(h);
                    return Err(e);
                }
            };
            lg.tx("commitment tx (on-chain output)", fc.target);
            lg.tx("forfeit tx (carries the RGB transition)", fc.forfeit_tx.compute_txid());
            mine_until(&regtest, &commit).await?;
            attest_and_register(&who, &w.ark, &w.rgb, &regtest, &fc, v, commit).await?;
            let carried: Vec<PathBuf> = h.proof.iter().map(PathBuf::from).collect();
            let mut ws = carried_witnesses(&carried)?;
            ws.push(fc.forfeit_tx.compute_txid());
            let cons = state.join(format!("{seq}-{who}-collab-exit.rgbc"));
            fc.transfer.save_file(&cons).map_err(|e| anyhow!("save: {e}"))?;
            let p = proof_path(&cons);
            build_proof(&w.ark, &regtest, &ws, &carried, &p).await?;
            import(&w.rgb, &w.online, &p, &ops)?;
            let _ = bl(|| w.rgb.update_witnesses(w.online.clone(), 0, vec![]));
            w.holdings.push(Holding { asset_id, amount: h.amount, kind: "exit-output".into(), outpoint: fc.target.to_string(), proof: Some(p.display().to_string()) });
            lg.push("ok", format!("{who}: {} now on on-chain output {}", h.amount, fc.target));
        }
        Action::UnilateralExit { who, asset_id, outpoint } => {
            let w = &mut c.wallets[idx(&who)?];
            let i = w
                .holdings
                .iter()
                .position(|x| x.asset_id == asset_id && x.kind == "vtxo" && outpoint.as_ref().is_none_or(|o| *o == x.outpoint))
                .ok_or_else(|| anyhow!("{who} holds no {asset_id} on a VTXO"))?;
            let v = OutPoint::from_str(&w.holdings[i].outpoint)?;
            lg.info(format!("fund {who}'s on-chain wallet for exit fees, then broadcast the branch of {v} (no operator needed)"));
            fund_onchain(&who, &w.ark, &w.ark_w).await?;
            let branch = unilateral_exit(&who, &w.ark, &w.ark_w, v.txid).await?;
            for t in &branch {
                lg.tx("exit branch tx", t);
            }
            let _ = bl(|| w.rgb.update_witnesses(w.online.clone(), 0, vec![]));
            w.holdings[i].kind = "unrolled".into();
            classify_unrolled(w, &regtest, &branch, v, lg).await?;
            lg.push("ok", format!("{who}: {v} is on-chain; spendable after the exit delay (on-chain send mines blocks until then)"));
        }
        Action::SweepBtc { who } => {
            let w = &mut c.wallets[idx(&who)?];
            ensure!(!w.exited_btc.is_empty(), "{who} has no exited BTC-only output");
            let outs: Vec<OutPoint> = w.exited_btc.iter().map(|o| OutPoint::from_str(o)).collect::<Result<_, _>>()?;
            let total: u64 = outs.iter().filter_map(|o| regtest.raw_tx(&o.txid).map(|t| t.output[o.vout as usize].value.to_sat())).sum();
            let fee = 600u64;
            ensure!(total > fee + 330, "{total} sat: too little to sweep");
            let to = w.ark.get_onchain_address()?;
            lg.info(format!("spend {} exited BTC-only output(s) ({total} sat) to {who}'s on-chain address {to} (mines blocks until the exit delay has passed)", outs.len()));
            let mut tx = None;
            for attempt in 0..120 {
                match w.ark.create_send_on_chain_transaction_with_hook(to.clone(), Amount::from_sat(total - fee), vec![], Amount::from_sat(fee), &outs, |_| Ok(())).await {
                    Ok((t, _)) => {
                        tx = Some(t);
                        break;
                    }
                    Err(e) => {
                        if attempt % 6 == 0 {
                            lg.info(format!("not spendable yet (attempt {attempt}): {e}"));
                        }
                        regtest::mine(1)?;
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    }
                }
            }
            let tx = tx.ok_or_else(|| anyhow!("exited outputs never became spendable"))?;
            let txid = regtest.send_raw(&tx)?;
            regtest::mine(1)?;
            lg.tx("sweep tx", txid);
            w.exited_btc.clear();
            lg.push("ok", format!("{who}: {} sat back in the on-chain wallet", total - fee));
        }
        Action::OnchainSend { from, to, asset_id, outpoint } => {
            let (fi, ti) = (idx(&from)?, idx(&to)?);
            let (f, t) = two_mut(&mut c.wallets, fi, ti)?;
            let h = take_holding(f, &asset_id, &["unrolled", "exit-output", "onchain"], &outpoint)?;
            let o = OutPoint::from_str(&h.outpoint)?;
            let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
            let recv = bl(|| t.rgb.witness_receive(None, Assignment::Fungible(h.amount), now() + 86_400, vec![], 1))?;
            let script = rgb_lib::utils::script_buf_from_recipient_id(recv.recipient_id.clone())?.ok_or_else(|| anyhow!("not a witness recipient"))?;
            let cons = state.join(format!("{seq}-{from}-to-{to}-onchain.rgbc"));
            let b = blinding();
            let res = if h.kind == "unrolled" {
                lg.info(format!("spend exited {o} to {to} (mines blocks until the exit delay has passed)"));
                let addr = Address::from_script(&script, Network::Regtest)?;
                spend_exited(&from, &f.ark, &f.rgb, &regtest, o, &addr, cid, h.amount, b, &cons).await
            } else {
                lg.info(format!("spend {o} (rgb-lib output) to {to}"));
                let value = regtest.raw_tx(&o.txid).and_then(|t| t.output.get(o.vout as usize).map(|x| x.value.to_sat())).unwrap_or(EXIT_SAT);
                pay_from_rgb_output(&from, &mut f.rgb, &f.online, &regtest, o, value, cid, h.amount, script, b, &cons).await
            };
            let txid = match res {
                Ok(x) => x,
                Err(e) => {
                    f.holdings.push(h);
                    return Err(e);
                }
            };
            lg.tx("on-chain RGB transfer", txid);
            regtest::mine(1)?;
            let carried: Vec<PathBuf> = h.proof.iter().map(PathBuf::from).collect();
            let mut ws = carried_witnesses(&carried)?;
            ws.push(txid);
            build_proof(&f.ark, &regtest, &ws, &carried, &proof_path(&cons)).await?;
            let settled = receive_onchain(&to, t, &cons, &ops, &asset_id, h.amount, lg).await?;
            // the Ark proof travels with the asset: its history may hold never-mined forfeits
            t.holdings.push(Holding { asset_id, amount: h.amount, kind: "onchain".into(), outpoint: format!("{txid}:1"), proof: Some(proof_path(&cons).display().to_string()) });
            lg.push("ok", format!("{to} settled {settled} on-chain"));
        }
        Action::Mine { blocks } => {
            let n = blocks.unwrap_or(1).min(500);
            regtest::mine(n)?;
            lg.push("ok", format!("mined {n} block(s)"));
        }
        Action::SetExpiry { seconds } => {
            let r = set_expiry(seconds)?;
            lg.info(format!("arkd admin: vtxoTreeExpiry={seconds} ({r}); batches started before keep the old expiry, wait ~45 s"));
            tokio::time::sleep(std::time::Duration::from_secs(45)).await;
            lg.push("ok", format!("new batches expire after {seconds} {}", if seconds < 512 { "blocks" } else { "s" }));
        }
        Action::Sync => {}
        Action::Reset => bail!("reset is handled by the worker"),
    }
    Ok(())
}

/// Accept an on-chain RGB transfer (consignment + carried Ark proof) and wait until it settles.
/// rgb-lib's stock balance also counts the wallet's (possibly already spent) Ark allocations, so
/// wait for an increase of `amount` over the balance before the transfer.
async fn receive_onchain(who: &str, t: &mut Wallet, cons: &Path, ops: &str, asset_id: &str, amount: u64, lg: &Logger) -> anyhow::Result<u64> {
    let p = proof_path(cons);
    let base = bl(|| t.rgb.get_asset_balance(asset_id.to_string())).map(|b| b.settled).unwrap_or(0);
    // the indexer behind rgb-lib's resolver can lag a few seconds behind the chain: retry
    let mut last = None;
    for i in 0..10 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        let r = (|| -> anyhow::Result<usize> {
            let n = if p.exists() { import(&t.rgb, &t.online, &p, ops)?.len() } else { 0 };
            bl(|| t.rgb.provide_out_of_band_consignment(t.online.clone(), cons.display().to_string(), vec![]))?;
            Ok(n)
        })();
        match r {
            Ok(n) => {
                lg.info(format!("{who} registered {n} Ark witnesses from the proof and accepted the consignment"));
                last = None;
                break;
            }
            Err(e) => {
                lg.info(format!("{who}: accept attempt {i} failed: {e:#}"));
                last = Some(e);
            }
        }
    }
    if let Some(e) = last {
        return Err(e.context(format!("{who} could not accept {}", cons.display())));
    }
    for _ in 0..10 {
        regtest::mine(1)?;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        bl(|| t.rgb.refresh(t.online.clone(), None, vec![], false))?;
        let b = bl(|| t.rgb.get_asset_balance(asset_id.to_string()))?;
        if b.settled >= base + amount {
            lg.info(format!("{who} stock rgb-lib balance {:?}", b));
            return Ok(amount);
        }
    }
    bail!("{who}: transfer of {amount} did not settle")
}

// --------------------------------------------------------------------------------------- views

async fn wallet_view(w: &Wallet, regtest: &Regtest) -> serde_json::Value {
    let addr = w.ark.get_offchain_address().await.map(|(a, _)| a.to_string()).unwrap_or_default();
    let bal = w.ark.offchain_balance().await.ok();
    let mut vtxos = vec![];
    if let Ok(l) = w.ark.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await {
        for v in l.spendable_offchain() {
            let x = v.vtxo();
            vtxos.push(serde_json::json!({"outpoint": x.outpoint.to_string(), "sat": x.amount.to_sat(), "expires_in": x.expires_at - now() as i64, "status": "spendable"}));
        }
    }
    if let Ok(l) = w.ark.list_vtxos_filtered(GetVtxosRequestFilter::Recoverable).await {
        for v in l.recoverable() {
            let x = v.vtxo();
            vtxos.push(serde_json::json!({"outpoint": x.outpoint.to_string(), "sat": x.amount.to_sat(), "expires_in": x.expires_at - now() as i64, "status": "swept/recoverable"}));
        }
    }
    let mut assets = vec![];
    if let Ok(a) = bl(|| w.rgb.list_assets(vec![])) {
        for n in a.nia.unwrap_or_default() {
            let ab = bl(|| w.rgb.get_asset_balance_ark(w.online.clone(), n.asset_id.clone())).ok();
            assets.push(serde_json::json!({
                "asset_id": n.asset_id, "ticker": n.ticker, "name": n.name,
                "onchain": {"settled": n.balance.settled, "future": n.balance.future, "spendable": n.balance.spendable},
                "ark": ab.map(|b| serde_json::json!({"active": b.active, "to_recover": b.to_recover, "unrolled": b.unrolled, "in_flight": b.in_flight})),
            }));
        }
    }
    // an asset received only off-chain is not in rgb-lib's stock asset list yet: show its Ark balance
    let mut extra: Vec<&str> = w.holdings.iter().map(|h| h.asset_id.as_str()).collect();
    extra.sort();
    extra.dedup();
    for id in extra {
        if assets.iter().any(|a| a["asset_id"] == id) {
            continue;
        }
        let ab = bl(|| w.rgb.get_asset_balance_ark(w.online.clone(), id.to_string())).ok();
        assets.push(serde_json::json!({
            "asset_id": id, "ticker": "", "name": "", "onchain": null,
            "ark": ab.map(|b| serde_json::json!({"active": b.active, "to_recover": b.to_recover, "unrolled": b.unrolled, "in_flight": b.in_flight})),
        }));
    }
    let mut holdings = vec![];
    for h in &w.holdings {
        let mut j = serde_json::to_value(h).unwrap_or_default();
        if h.kind == "vtxo" {
            if let Ok(o) = OutPoint::from_str(&h.outpoint) {
                if let Some(i) = vtxo_info(o).await {
                    j["expires_in"] = (i.expires_at - now() as i64).into();
                    j["swept"] = i.is_swept.into();
                    j["spent"] = i.is_spent.into();
                }
            }
        } else if let Ok(o) = OutPoint::from_str(&h.outpoint) {
            j["confirmations"] = regtest.tx_confirmations(&o.txid).into();
        }
        holdings.push(j);
    }
    serde_json::json!({
        "name": w.name,
        "ark_address": addr,
        "btc": bal.map(|b| serde_json::json!({"offchain_confirmed": b.confirmed().to_sat(), "offchain_preconfirmed": b.pre_confirmed().to_sat(), "recoverable": b.recoverable().to_sat()})),
        "vtxos": vtxos,
        "assets": assets,
        "holdings": holdings,
        "exited_btc": w.exited_btc,
    })
}

async fn refresh_views(c: &Core, shared: &Arc<StdMutex<Shared>>) {
    let mut views = vec![];
    for w in &c.wallets {
        views.push(wallet_view(w, &c.regtest).await);
    }
    let height = c.regtest.rpc("getblockcount", serde_json::json!([])).ok().and_then(|v| v.as_u64()).unwrap_or_default();
    let mut s = shared.lock().unwrap();
    s.wallets = views;
    s.height = height;
}

// ---------------------------------------------------------------------------------------- http

async fn index() -> Html<&'static str> {
    Html(INDEX)
}

/// The caller's session: the single local one, or (public mode) the one named by the cookie.
/// With `create`, a visitor without a live session gets a new one under a fresh server-side id
/// (never the id the client sent): the least recently used session makes room when the demo is
/// full or the visitor's address already holds `RGB_ARKADE_SESSIONS_PER_IP` (3). The second value
/// is the Set-Cookie header for a new session.
fn session(app: &AppState, headers: &HeaderMap, create: bool) -> Result<Option<(Arc<Session>, Option<String>)>, (StatusCode, String)> {
    let mut map = app.sessions.lock().unwrap();
    if !app.public {
        return map.get(LOCAL).cloned().map(|s| Some((s, None))).ok_or((StatusCode::SERVICE_UNAVAILABLE, "starting".into()));
    }
    let sid = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').find_map(|kv| kv.trim().strip_prefix(&format!("{COOKIE}="))));
    if let Some(s) = sid.and_then(|id| map.get(id)) {
        s.last_seen.store(now(), Ordering::Relaxed);
        return Ok(Some((s.clone(), None)));
    }
    if !create {
        return Ok(None);
    }
    // The app listens on loopback behind Caddy, which appends the client address as the last
    // X-Forwarded-For entry; IPv6 clients are grouped by /64. No header = a request from the host.
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .map(|v| match v.trim().parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(a)) if a.to_ipv4_mapped().is_none() => {
                let g = a.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", g[0], g[1], g[2], g[3])
            }
            Ok(std::net::IpAddr::V6(a)) => a.to_ipv4_mapped().map(|v4| v4.to_string()).unwrap_or_default(),
            Ok(std::net::IpAddr::V4(a)) => a.to_string(),
            Err(_) => "unparsed".into(),
        })
        .unwrap_or_else(|| "host".into());
    let lru = |map: &HashMap<String, Arc<Session>>, ip: Option<&str>| {
        map.iter().filter(|(_, s)| ip.is_none_or(|ip| s.ip == ip)).min_by_key(|(_, s)| s.last_seen.load(Ordering::Relaxed)).map(|(k, _)| k.clone())
    };
    {
        let mut created = app.created.lock().unwrap();
        if now().saturating_sub(created.get(&ip).copied().unwrap_or(0)) < env_u64("RGB_ARKADE_NEW_SESSION_EVERY", 20) {
            return Err((StatusCode::TOO_MANY_REQUESTS, "new wallets were just created from your address: wait a few seconds".into()));
        }
        created.retain(|_, t| now().saturating_sub(*t) < 3600);
        created.insert(ip.clone(), now());
    }
    if map.values().filter(|s| s.ip == ip).count() >= env_u64("RGB_ARKADE_SESSIONS_PER_IP", 3) as usize {
        if let Some(k) = lru(&map, Some(&ip)) {
            map.remove(&k);
        }
    }
    let max = env_u64("RGB_ARKADE_MAX_SESSIONS", 40) as usize;
    if map.len() >= max {
        // only a visitor gone for a while makes room for a new one
        match lru(&map, None) {
            Some(k) if now().saturating_sub(map[&k].last_seen.load(Ordering::Relaxed)) > 600 => {
                map.remove(&k);
            }
            _ => return Err((StatusCode::SERVICE_UNAVAILABLE, "the demo is full right now, try again in a few minutes".into())),
        }
    }
    if LIVE_WORKERS.load(Ordering::Relaxed) >= max + 5 {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "the demo is busy right now, try again in a few minutes".into()));
    }
    let id: String = rand::random::<[u8; 16]>().iter().map(|b| format!("{b:02x}")).collect();
    let s = spawn_session(data_dir().join("sessions").join(&id), true, app.public, app.explorer.clone(), ip);
    map.insert(id.clone(), s.clone());
    Ok(Some((s, Some(format!("{COOKIE}={id}; Path=/; Max-Age=604800; HttpOnly; SameSite=Lax")))))
}

fn with_cookie(cookie: Option<String>) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(c) = cookie.and_then(|c| c.parse().ok()) {
        h.insert(header::SET_COOKIE, c);
    }
    h
}

async fn get_state(State(app): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    match session(&app, &headers, false) {
        Ok(Some((s, cookie))) => {
            let mut view = s.shared.lock().unwrap().clone();
            if app.public {
                // the server's directories are nobody else's business
                let dirs = [view.state_dir.clone(), crate::paths::root().display().to_string()];
                for e in &mut view.log {
                    for d in dirs.iter().filter(|d| !d.is_empty()) {
                        e.msg = e.msg.replace(&format!("{d}/"), "").replace(d.as_str(), "your wallet folder");
                    }
                }
                view.state_dir.clear();
            }
            (StatusCode::OK, with_cookie(cookie), Json(serde_json::to_value(view).unwrap_or_default()))
        }
        // public visitor without wallets yet: they are created by the first action
        Ok(None) => (StatusCode::OK, HeaderMap::new(), Json(serde_json::to_value(Shared { public: true, explorer: app.explorer.clone(), ..Default::default() }).unwrap_or_default())),
        Err((code, e)) => (code, HeaderMap::new(), Json(serde_json::json!({"ok": false, "error": e}))),
    }
}

async fn post_action(State(app): State<AppState>, headers: HeaderMap, Json(a): Json<Action>) -> impl IntoResponse {
    let err = |code: StatusCode, e: String| (code, HeaderMap::new(), Json(serde_json::json!({"ok": false, "error": e})));
    let label = a.label();
    if app.public && matches!(a, Action::SetExpiry { .. } | Action::Mine { blocks: Some(7..) }) {
        return err(StatusCode::FORBIDDEN, "not available on the public demo".into());
    }
    let sess = match session(&app, &headers, true) {
        // new visitor: the wallets are being created; the action itself is not queued
        Ok(Some((_, Some(cookie)))) => return (StatusCode::ACCEPTED, with_cookie(Some(cookie)), Json(serde_json::json!({"ok": true, "started": "creating your wallets"}))),
        Ok(Some((s, None))) => s,
        Ok(None) => unreachable!("create = true"),
        Err((code, e)) => return err(code, e),
    };
    {
        let mut s = sess.shared.lock().unwrap();
        if let Some(b) = &s.busy {
            return err(StatusCode::CONFLICT, format!("busy: {b}"));
        }
        s.busy = Some(label.clone());
    }
    if sess.jobs.lock().unwrap().send(a).is_err() {
        // the worker is gone (its wallets could not be created): forget the session so the next
        // action starts a new one instead of hitting the dead one until the cookie expires
        app.sessions.lock().unwrap().retain(|_, s| !Arc::ptr_eq(s, &sess));
        return err(StatusCode::SERVICE_UNAVAILABLE, "your wallets could not be created: try again in a few seconds".into());
    }
    (StatusCode::ACCEPTED, HeaderMap::new(), Json(serde_json::json!({"ok": true, "started": label})))
}

/// Data dir: `--data-dir=<path>`, else `$RGB_ARKADE_APP_DATA`, else `state/app-data`.
fn data_dir() -> PathBuf {
    std::env::args()
        .find_map(|a| a.strip_prefix("--data-dir=").map(PathBuf::from))
        .or_else(|| std::env::var_os("RGB_ARKADE_APP_DATA").map(PathBuf::from))
        .unwrap_or_else(|| crate::paths::state().join("app-data"))
}

/// Marker file of a data dir created by the app or the CLI: Reset deletes only such dirs.
const DATA_DIR_MARKER: &str = ".rgb-arkade-data";

/// Make `dir` the app's data dir: create it, or accept it if it is empty, already marked, or holds
/// wallets of an earlier version; refuse any other non-empty dir (Reset would delete it).
fn claim_data_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let marker = dir.join(DATA_DIR_MARKER);
    if marker.exists() {
        return Ok(());
    }
    let empty = std::fs::read_dir(dir)?.next().is_none();
    let legacy = dir.join(format!("{}-ark-seed.hex", NAMES[0])).exists();
    if !empty && !legacy {
        bail!("refusing to use {}: it is not empty and was not created by ark-rgb-demo (no {DATA_DIR_MARKER}); choose an empty or new dir", dir.display());
    }
    std::fs::write(marker, "rgb-arkade app/CLI data dir: Reset deletes this whole directory\n")?;
    Ok(())
}

/// Delete a data dir, only if it carries the marker.
fn remove_data_dir(dir: &Path) -> anyhow::Result<()> {
    if !dir.join(DATA_DIR_MARKER).exists() {
        bail!("refusing to delete {}: no {DATA_DIR_MARKER}, not an ark-rgb-demo data dir", dir.display());
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

/// Open the wallets in `state` if it holds them (app restart), else create fresh ones there.
async fn init_in(shared: &Arc<StdMutex<Shared>>, state: PathBuf) -> anyhow::Result<Core> {
    claim_data_dir(&state)?;
    shared.lock().unwrap().state_dir = state.display().to_string();
    let regtest = Arc::new(Regtest::new());
    let lg = Logger { shared: shared.clone(), action: "startup".into() };
    let existing = state.join(format!("{}-ark-seed.hex", NAMES[0])).exists();
    let saved: Saved = match std::fs::read(state.join(SAVED)) {
        Ok(b) if existing => serde_json::from_slice(&b)?,
        _ => Saved::default(),
    };
    if existing {
        shared.lock().unwrap().log = saved.log;
    }
    let mut wallets = vec![];
    for n in NAMES {
        let mut w = if existing { load_wallet(n, &regtest, &state).await? } else { new_wallet(n, &regtest, &state).await? };
        w.holdings = saved.holdings.get(n).cloned().unwrap_or_default();
        w.exited_btc = saved.exited_btc.get(n).cloned().unwrap_or_default();
        lg.info(format!("wallet {n} {} (Ark client + rgb-lib wallet in {})", if existing { "reloaded" } else { "created" }, state.display()));
        wallets.push(w);
    }
    let info = wallets[0].ark.server_info().await?;
    shared.lock().unwrap().arkd = serde_json::json!({
        "version": info.version, "signer": info.signer_pk.to_string(), "dust": info.dust.to_sat(),
        "unilateral_exit_delay": format!("{:?}", info.unilateral_exit_delay),
        "exit_delay": match info.unilateral_exit_delay.to_relative_lock_time() {
            Some(bitcoin::relative::LockTime::Blocks(h)) => format!("{} blocks", h.value()),
            Some(bitcoin::relative::LockTime::Time(t)) => format!("{} s", u32::from(t.value()) * 512),
            None => "?".into(),
        },
    });
    let core = Core { state, regtest, ops: operators(&info), wallets, seq: saved.seq };
    save(&core, shared)?;
    refresh_views(&core, shared).await;
    Ok(core)
}

/// The wallets (rgb-lib, Ark client) live on one worker thread with its own runtime; actions are
/// queued to it and run one at a time (the existing flow helpers are not `Send`).
fn worker(shared: Arc<StdMutex<Shared>>, rx: std::sync::mpsc::Receiver<Action>, dir: PathBuf, ephemeral: bool, ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>) {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    rt.block_on(async move {
        let started = {
            let _turn = tokio::task::block_in_place(|| ACTION_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
            // evicted while waiting for its turn: nothing to create
            if matches!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)) {
                return;
            }
            // the electrum indexer flakes ("Invalid indexer") under concurrent load: retry
            let mut r = init_in(&shared, dir.clone()).await;
            for _ in 0..10 {
                let Err(e) = &r else { break };
                Logger { shared: shared.clone(), action: "startup".into() }.info(format!("retrying in 5 s: {e:#}"));
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                r = init_in(&shared, dir.clone()).await;
            }
            r
        };
        shared.lock().unwrap().busy = None;
        let mut core = match started {
            Ok(c) => {
                let _ = ready.send(Ok(()));
                c
            }
            Err(e) => {
                Logger { shared: shared.clone(), action: "startup".into() }.push("error", format!("{e:#}"));
                let _ = ready.send(Err(e));
                return;
            }
        };
        while let Ok(a) = tokio::task::block_in_place(|| rx.recv()) {
            let label = a.label();
            if matches!(a, Action::Reset) {
                let dir = core.state.clone();
                drop(core);
                let r = remove_data_dir(&dir);
                shared.lock().unwrap().log.clear();
                core = loop {
                    match init_in(&shared, dir.clone()).await {
                        Ok(c) => break c,
                        Err(e) => {
                            Logger { shared: shared.clone(), action: "startup".into() }.push("error", format!("{e:#}"));
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                };
                let lg = Logger { shared: shared.clone(), action: label.clone() };
                lg.push("start", label.clone());
                match r {
                    Ok(()) => {
                        lg.push("ok", format!("deleted {} and started fresh wallets", dir.display()));
                        lg.push("done", label.clone());
                    }
                    Err(e) => lg.push("error", format!("could not delete {}: {e:#}", dir.display())),
                }
                let _ = save(&core, &shared);
                shared.lock().unwrap().busy = None;
                continue;
            }
            let lg = Logger { shared: shared.clone(), action: label.clone() };
            lg.push("start", label.clone());
            let turn = ACTION_LOCK.try_lock().ok();
            let _turn = match turn {
                Some(t) => t,
                None => {
                    lg.info("waiting for other visitors' actions to finish");
                    tokio::task::block_in_place(|| ACTION_LOCK.lock().unwrap_or_else(|e| e.into_inner()))
                }
            };
            match run_action(&mut core, &a, &lg).await {
                Ok(()) => lg.push("done", label.clone()),
                Err(e) => lg.push("error", format!("{e:#}")),
            }
            if let Err(e) = save(&core, &shared) {
                lg.push("error", format!("saving app state: {e:#}"));
            }
            refresh_views(&core, &shared).await;
            drop(_turn);
            shared.lock().unwrap().busy = None;
        }
        // session dropped (idle): its wallets are throwaway regtest wallets
        drop(core);
        if ephemeral {
            let _ = remove_data_dir(&dir);
        }
    });
}

fn spawn_session(dir: PathBuf, ephemeral: bool, public: bool, explorer: String, ip: String) -> Arc<Session> {
    let shared = Arc::new(StdMutex::new(Shared { busy: Some("creating your wallets".into()), public, explorer, ..Default::default() }));
    let (tx, rx) = std::sync::mpsc::channel::<Action>();
    let (ready_tx, _ready) = tokio::sync::oneshot::channel();
    let sh = shared.clone();
    struct Live;
    impl Drop for Live {
        fn drop(&mut self) {
            LIVE_WORKERS.fetch_sub(1, Ordering::Relaxed);
        }
    }
    LIVE_WORKERS.fetch_add(1, Ordering::Relaxed);
    std::thread::spawn(move || {
        let _live = Live;
        worker(sh, rx, dir, ephemeral, ready_tx);
    });
    Arc::new(Session { jobs: StdMutex::new(tx), shared, last_seen: AtomicU64::new(now()), ip })
}

/// `--public` (or `RGB_ARKADE_PUBLIC=1`): one wallet pair per visitor (cookie), dropped after
/// `RGB_ARKADE_SESSION_TTL` s idle (default 2 h), at most `RGB_ARKADE_MAX_SESSIONS` (40); regtest
/// controls off except mining a few blocks; a background miner every `RGB_ARKADE_AUTOMINE` s (60).
/// `RGB_ARKADE_EXPLORER`: tx URL prefix for the explorer links; `RGB_ARKADE_BIND`: listen address.
pub async fn run(port: u16) -> anyhow::Result<()> {
    let public = std::env::args().any(|a| a == "--public") || std::env::var("RGB_ARKADE_PUBLIC").is_ok_and(|v| v == "1");
    let explorer = std::env::var("RGB_ARKADE_EXPLORER").unwrap_or_default();
    let sessions: Arc<StdMutex<HashMap<String, Arc<Session>>>> = Arc::default();
    if public {
        // sessions do not survive a restart (they live in memory), so neither do their wallets
        let _ = std::fs::remove_dir_all(data_dir().join("sessions"));
        let clock = Arc::new(StdMutex::new(Shared { automine: env_u64("RGB_ARKADE_AUTOMINE", 60), ..Default::default() }));
        std::thread::spawn(move || miner(clock));
        let ss = sessions.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(300));
            let ttl = env_u64("RGB_ARKADE_SESSION_TTL", 7200);
            ss.lock().unwrap().retain(|_, s| now().saturating_sub(s.last_seen.load(Ordering::Relaxed)) < ttl);
        });
    } else {
        let s = spawn_session(data_dir(), false, false, explorer.clone(), String::new());
        let sh = s.shared.clone();
        std::thread::spawn(move || miner(sh));
        while s.shared.lock().unwrap().busy.is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        sessions.lock().unwrap().insert(LOCAL.into(), s);
    }
    let app = AppState { sessions, created: Arc::default(), public, explorer };
    let router = axum::Router::new()
        .route("/", get(index))
        .route("/api/state", get(get_state))
        .route("/api/action", post(post_action))
        .route("/api/automine", post(post_automine))
        .with_state(app);
    let bind = std::env::var("RGB_ARKADE_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let listener = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
    println!("rgb-arkade app on http://{bind}:{port}{}", if public { " (public: one wallet pair per visitor)" } else { "" });
    axum::serve(listener, router).await?;
    Ok(())
}

// ---------------------------------------------------------------------------------- regression

fn holding_out(c: &Core, who: &str, asset_id: &str, amount: u64, kind: &str) -> anyhow::Result<String> {
    c.wallets[idx(who)?]
        .holdings
        .iter()
        .find(|h| h.asset_id == asset_id && h.amount == amount && h.kind == kind)
        .map(|h| h.outpoint.clone())
        .ok_or_else(|| anyhow!("{who} has no {kind} holding of {amount}"))
}

/// Scenario `multihop-exit` (regression): the app flows, headless. An asset goes through two
/// off-chain hops (Alice -> Bob -> Alice), Alice exits her 700 change collaboratively (that VTXO is
/// forfeited), then unilaterally exits the 100 she got back and spends it on-chain. Unrolling the
/// 100 also puts the parent Arkade tx on-chain, whose forfeited 700 change output pays Alice's own
/// VTXO script and matures first: the exit spend must still spend the colored output.
pub async fn run_multihop_exit() -> anyhow::Result<()> {
    let shared = Arc::new(StdMutex::new(Shared::default()));
    let dir = crate::paths::state().join(format!("run-{}-multihop-exit", now()));
    println!("state dir {}", dir.display());
    let mut core = init_in(&shared, dir).await?;
    let act = async |core: &mut Core, a: Action| -> anyhow::Result<()> {
        let lg = Logger { shared: shared.clone(), action: a.label() };
        crate::step(&a.label());
        run_action(core, &a, &lg).await.map_err(|e| e.context(a.label()))
    };
    let (alice, bob) = ("alice".to_string(), "bob".to_string());
    act(&mut core, Action::Board { who: alice.clone(), sats: None }).await?;
    act(&mut core, Action::Board { who: bob.clone(), sats: None }).await?;
    act(&mut core, Action::Issue { who: alice.clone() }).await?;
    let a = core.wallets[0].holdings[0].asset_id.clone();
    act(&mut core, Action::Send { from: alice.clone(), to: bob.clone(), asset_id: a.clone(), amount: 300, sats: None, outpoint: None }).await?;
    let back = holding_out(&core, "bob", &a, 300, "vtxo")?;
    act(&mut core, Action::Send { from: bob.clone(), to: alice.clone(), asset_id: a.clone(), amount: 100, sats: None, outpoint: Some(back) }).await?;
    let change = holding_out(&core, "alice", &a, 700, "vtxo")?;
    act(&mut core, Action::CollabExit { who: alice.clone(), asset_id: a.clone(), outpoint: Some(change.clone()) }).await?;
    let v = holding_out(&core, "alice", &a, 100, "vtxo")?;
    println!("multihop_exit_vtxo = {v} (parent Arkade tx also holds alice's forfeited change {change})");
    let forfeit_txid = shared.lock().unwrap().log.iter().rev()
        .find_map(|e| e.msg.strip_prefix("forfeit tx (carries the RGB transition): ").map(str::to_string))
        .ok_or_else(|| anyhow!("collab exit logged no forfeit tx"))?;
    let h0 = core.regtest.height();
    act(&mut core, Action::UnilateralExit { who: alice.clone(), asset_id: a.clone(), outpoint: Some(v.clone()) }).await?;
    let r = act(&mut core, Action::OnchainSend { from: alice.clone(), to: bob.clone(), asset_id: a.clone(), outpoint: Some(v.clone()) }).await;
    let spend_ok = r.is_ok();
    if let Err(e) = &r {
        println!("on-chain spend of the unrolled VTXO FAILED: {e:#}");
    }
    let view = wallet_view(&core.wallets[0], &core.regtest).await;
    let ark = view["assets"].as_array().and_then(|l| l.iter().find(|x| x["asset_id"] == a.as_str())).map(|x| x["ark"].clone()).unwrap_or_default();
    println!("alice Ark balance after the exit spend: {ark}");
    let unrolled_clear = ark["unrolled"].as_u64() == Some(0);
    let bob_has = core.wallets[1].holdings.iter().any(|h| h.asset_id == a && h.kind == "onchain" && h.amount == 100);
    // the unroll also put alice's forfeited change on-chain: the operator must react to the fraud
    // by broadcasting the (colored) forfeit tx stored for it, which spends that output
    let reacted = operator_forfeit_spend(&core.regtest, h0, &change).await?;
    println!("forfeited change {change}: spent on-chain by {reacted:?} (stored forfeit {forfeit_txid})");
    let operator_claimed = reacted.as_deref() == Some(forfeit_txid.as_str());
    println!("check_exit_spend_ok = {spend_ok}\ncheck_unrolled_balance_cleared = {unrolled_clear}\ncheck_bob_holds_100_onchain = {bob_has}\ncheck_operator_broadcast_forfeit = {operator_claimed}");
    let pass = spend_ok && unrolled_clear && bob_has && operator_claimed;
    println!("\nMULTIHOP-EXIT RESULT: {}", if pass { "PASS" } else { "FAIL" });
    ensure!(pass, "multihop-exit failed");
    Ok(())
}

/// Txid of the confirmed tx (above height `from`) spending `outpoint`, mining a few blocks while
/// the operator reacts; None if it stays unspent.
async fn operator_forfeit_spend(regtest: &crate::regtest::Regtest, from: u64, outpoint: &str) -> anyhow::Result<Option<String>> {
    let (txid, vout) = outpoint.split_once(':').ok_or_else(|| anyhow!("bad outpoint {outpoint}"))?;
    let vout: u64 = vout.parse()?;
    for _ in 0..12 {
        let unspent = regtest.rpc("gettxout", serde_json::json!([txid, vout, false]))?;
        if unspent.is_null() {
            for h in from..=regtest.height() {
                let hash = regtest.rpc("getblockhash", serde_json::json!([h]))?;
                let block = regtest.rpc("getblock", serde_json::json!([hash, 2]))?;
                for tx in block["tx"].as_array().into_iter().flatten() {
                    let spends = tx["vin"].as_array().into_iter().flatten()
                        .any(|i| i["txid"].as_str() == Some(txid) && i["vout"].as_u64() == Some(vout));
                    if spends {
                        return Ok(tx["txid"].as_str().map(str::to_string));
                    }
                }
            }
        }
        crate::regtest::mine(1)?;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    Ok(None)
}

const CLI_HELP: &str = "commands (wallets: alice, bob):
  board <who> [sats]              board bitcoin into a batch (default 100000 sat)
  issue <who>                     issue 1000 units on-chain and jump them onto a VTXO
  balance [who]                   bitcoin, VTXOs and assets
  send <from> <to> <amount>       off-chain transfer in one Arkade tx
  swap <seller> <buyer> <amount> <price>   asset for bitcoin, atomically, in one Arkade tx
  refresh <who>                   move the colored VTXO into a new batch
  collab-exit <who>               leave to an on-chain output with the operator
  exit <who>                      unilateral exit: put the colored VTXO on-chain alone
  onchain-send <from> <to>        spend an exited asset on-chain to the other wallet
  recover <who>                   recover a VTXO swept after expiry, asset included
  mine [n]                        mine n blocks (default 1)
  reset                           start again with two fresh wallets
  help | quit";

/// The asset a wallet holds (the first one; the CLI issues one per wallet).
fn cli_asset(c: &Core, who: &str) -> anyhow::Result<String> {
    c.wallets[idx(who)?].holdings.first().map(|h| h.asset_id.clone()).ok_or_else(|| anyhow!("{who} holds no asset yet (issue {who})"))
}

fn cli_parse(c: &Core, line: &str) -> anyhow::Result<Option<Action>> {
    let w: Vec<&str> = line.split_whitespace().collect();
    let arg = |i: usize| -> anyhow::Result<String> { w.get(i).map(|s| s.to_string()).ok_or_else(|| anyhow!("missing argument; type help")) };
    let num = |i: usize| -> anyhow::Result<u64> { Ok(arg(i)?.parse()?) };
    Ok(Some(match w.first().copied().unwrap_or("") {
        "board" => Action::Board { who: arg(1)?, sats: w.get(2).map(|s| s.parse()).transpose()? },
        "issue" => Action::Issue { who: arg(1)? },
        "send" => Action::Send { from: arg(1)?, to: arg(2)?, asset_id: cli_asset(c, &arg(1)?)?, amount: num(3)?, sats: None, outpoint: None },
        "swap" => Action::Swap { seller: arg(1)?, buyer: arg(2)?, asset_id: cli_asset(c, &arg(1)?)?, amount: num(3)?, price: num(4)?, outpoint: None },
        "refresh" => Action::Refresh { who: arg(1)?, asset_id: cli_asset(c, &arg(1)?)?, outpoint: None },
        "collab-exit" => Action::CollabExit { who: arg(1)?, asset_id: cli_asset(c, &arg(1)?)?, outpoint: None },
        "exit" => Action::UnilateralExit { who: arg(1)?, asset_id: cli_asset(c, &arg(1)?)?, outpoint: None },
        "onchain-send" => Action::OnchainSend { from: arg(1)?, to: arg(2)?, asset_id: cli_asset(c, &arg(1)?)?, outpoint: None },
        "recover" => Action::Recover { who: arg(1)?, asset_id: cli_asset(c, &arg(1)?)?, outpoint: None },
        "mine" => Action::Mine { blocks: w.get(1).map(|s| s.parse()).transpose()? },
        "reset" => Action::Reset,
        "" => return Ok(None),
        other => bail!("unknown command {other}; type help"),
    }))
}

async fn cli_balance(c: &Core, who: Option<&str>) -> anyhow::Result<()> {
    for w in c.wallets.iter().filter(|w| who.is_none_or(|n| n == w.name)) {
        let v = wallet_view(w, &c.regtest).await;
        let btc = &v["btc"];
        println!("{}: {} sat off-chain ({} preconfirmed)", w.name, btc["offchain_confirmed"].as_u64().unwrap_or(0) + btc["offchain_preconfirmed"].as_u64().unwrap_or(0), btc["offchain_preconfirmed"].as_u64().unwrap_or(0));
        for x in v["vtxos"].as_array().into_iter().flatten() {
            println!("  vtxo  {}  {} sat  {}", x["outpoint"].as_str().unwrap_or(""), x["sat"], x["status"].as_str().unwrap_or(""));
        }
        for h in &w.holdings {
            println!("  asset {}  {} units  {}  {}", &h.asset_id[..h.asset_id.len().min(22)], h.amount, h.kind, h.outpoint);
        }
    }
    Ok(())
}

/// `ark-rgb-demo cli [dir]`: an interactive shell over the same flows as the app, for two
/// wallets (alice, bob) kept in `dir` (default `state/cli`).
pub async fn run_cli(dir: Option<PathBuf>) -> anyhow::Result<()> {
    CLI.store(true, Ordering::Relaxed);
    let dir = dir.unwrap_or_else(|| crate::paths::state().join("cli"));
    claim_data_dir(&dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("cli.log"))?;
    let log_fd = std::os::fd::IntoRawFd::into_raw_fd(log);
    // SAFETY: duplicating stdout so the CLI can keep writing to the terminal
    CLI_TTY.store(unsafe { libc::dup(1) }, Ordering::Relaxed);
    let shared = Arc::new(StdMutex::new(Shared::default()));
    let mut core = init_in(&shared, dir.clone()).await?;
    let info = core.wallets[0].ark.server_info().await?;
    println!("RGB on Arkade: arkd {} at {}, wallets alice and bob in {}", info.version, crate::paths::arkd_url(), dir.display());
    println!("type help for commands (details of every step go to {})", dir.join("cli.log").display());
    let stdin = std::io::stdin();
    loop {
        print!("rgb-ark> ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        match line.split_whitespace().next() {
            _ if line == "quit" => break,
            Some("help") => println!("{CLI_HELP}"),
            Some("balance") => {
                if let Err(e) = cli_balance(&core, line.split_whitespace().nth(1)).await {
                    println!("error: {e:#}");
                }
            }
            _ => match cli_parse(&core, line) {
                Ok(None) => {}
                Ok(Some(Action::Reset)) => {
                    if let Err(e) = remove_data_dir(&dir) {
                        println!("error: {e:#}");
                        continue;
                    }
                    *shared.lock().unwrap() = Shared::default();
                    claim_data_dir(&dir)?;
                    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("cli.log"))?;
                    // SAFETY: point the log fd at the new file
                    unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&log), log_fd) };
                    core = init_in(&shared, dir.clone()).await?;
                    println!("done: two fresh wallets in {}", dir.display());
                }
                Ok(Some(a)) => {
                    let lg = Logger { shared: shared.clone(), action: a.label() };
                    let t = std::time::Instant::now();
                    std::io::Write::flush(&mut std::io::stdout())?;
                    // SAFETY: plain fd juggling on stdout; restored right after the action
                    unsafe { libc::dup2(log_fd, 1) };
                    let r = run_action(&mut core, &a, &lg).await;
                    std::io::Write::flush(&mut std::io::stdout())?;
                    unsafe { libc::dup2(CLI_TTY.load(Ordering::Relaxed), 1) };
                    match r {
                        Ok(()) => println!("done: {} ({:.0} s)", a.label(), t.elapsed().as_secs_f64()),
                        Err(e) => println!("error: {e:#}"),
                    }
                    save(&core, &shared)?;
                }
                Err(e) => println!("error: {e:#}"),
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rgb-arkade-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn reset_deletes_only_marked_data_dirs() {
        // new dir: claimed, then deletable
        let d = tmp("new");
        claim_data_dir(&d).unwrap();
        std::fs::write(d.join("app.json"), "{}").unwrap();
        claim_data_dir(&d).unwrap();
        remove_data_dir(&d).unwrap();
        assert!(!d.exists());

        // a non-empty dir that is not ours: neither used nor deleted
        let f = tmp("foreign");
        std::fs::create_dir_all(&f).unwrap();
        std::fs::write(f.join("notes.txt"), "keep me").unwrap();
        assert!(claim_data_dir(&f).is_err());
        assert!(remove_data_dir(&f).is_err());
        assert!(f.join("notes.txt").exists());
        std::fs::remove_dir_all(&f).unwrap();

        // wallets of an earlier version (no marker yet): adopted
        let l = tmp("legacy");
        std::fs::create_dir_all(&l).unwrap();
        std::fs::write(l.join("alice-ark-seed.hex"), "00").unwrap();
        claim_data_dir(&l).unwrap();
        remove_data_dir(&l).unwrap();
        assert!(!l.exists());
    }
}

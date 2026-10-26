//! `ark-rgb-demo app [port]`: prototype web app (regtest). Two wallets (Alice, Bob) side by side;
//! every RGB-on-Arkade flow is one button / one `POST /api/action`. Actions run one at a time in
//! the background; `GET /api/state` returns balances, VTXOs, RGB holdings and the activity log.
use crate::batch_scenarios::{forfeit_proof, own};
use crate::forfeit::{BatchOp, attest_and_register, colored_batch, fund_onchain, mine_until, spend_exited, unilateral_exit};
use crate::long_offline::{pay_from_rgb_output, vtxo_info};
use crate::offchain_transfer::{rgb_hop, wait_vtxo};
use crate::proof::{ProofFile, add_vtxos, arkd_down, build_proof, import, mark_recovery, negatives, operators, proof_path};
use crate::{ArkClient, ark_client, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet};
use anyhow::{anyhow, bail, ensure};
use ark_bdk_wallet::Wallet as ArkBdkWallet;
use ark_client::GetVtxosRequestFilter;
use ark_core::send::SendReceiver;
use axum::extract::State;
use axum::http::StatusCode;
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
use std::sync::{Arc, Mutex as StdMutex};

const INDEX: &str = include_str!("../app/index.html");
const NAMES: [&str; 2] = ["alice", "bob"];
const EXIT_SAT: u64 = 20_000;

#[derive(Serialize, Clone, Debug)]
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
}

#[derive(Serialize, Clone, Default)]
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
}

struct Core {
    state: PathBuf,
    regtest: Arc<Regtest>,
    ops: String,
    wallets: Vec<Wallet>,
    seq: u64,
}

#[derive(Clone)]
struct AppState {
    jobs: Arc<StdMutex<std::sync::mpsc::Sender<Action>>>,
    shared: Arc<StdMutex<Shared>>,
}

#[derive(Deserialize)]
struct AutoMine {
    seconds: u64,
}

/// Regtest time control that runs beside the action queue: mine one block every `seconds`.
async fn post_automine(State(app): State<AppState>, Json(m): Json<AutoMine>) -> impl IntoResponse {
    let mut s = app.shared.lock().unwrap();
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
    Send { from: String, to: String, asset_id: String, amount: u64, sats: Option<u64> },
    Refresh { who: String, asset_id: String },
    CollabExit { who: String, asset_id: String },
    UnilateralExit { who: String, asset_id: String },
    OnchainSend { from: String, to: String, asset_id: String },
    Recover { who: String, asset_id: String },
    Mine { blocks: Option<u32> },
    SetExpiry { seconds: u64 },
    Sync,
}

impl Action {
    fn label(&self) -> String {
        match self {
            Action::Board { who, .. } => format!("{who}: board BTC"),
            Action::Issue { who } => format!("{who}: issue asset + jump-in"),
            Action::Send { from, to, amount, .. } => format!("{from} -> {to}: send {amount} off-chain"),
            Action::Refresh { who, .. } => format!("{who}: refresh in a batch"),
            Action::CollabExit { who, .. } => format!("{who}: collaborative exit"),
            Action::UnilateralExit { who, .. } => format!("{who}: unilateral exit"),
            Action::OnchainSend { from, to, .. } => format!("{from} -> {to}: on-chain send"),
            Action::Recover { who, .. } => format!("{who}: sweep recovery"),
            Action::Mine { blocks } => format!("mine {} block(s)", blocks.unwrap_or(1)),
            Action::SetExpiry { seconds } => format!("set batch expiry {seconds}s"),
            Action::Sync => "sync".into(),
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
        println!("[app] [{}] {kind}: {msg}", self.action);
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

fn take_holding(w: &mut Wallet, asset_id: &str, kinds: &[&str]) -> anyhow::Result<Holding> {
    let i = w
        .holdings
        .iter()
        .position(|h| h.asset_id == asset_id && kinds.contains(&h.kind.as_str()))
        .ok_or_else(|| anyhow!("{} holds no {asset_id} as {kinds:?}", w.name))?;
    Ok(w.holdings.remove(i))
}

fn blinding() -> u64 {
    (rand::random::<u32>() as u64) + 1
}

fn carried_witnesses(carried: &[PathBuf]) -> anyhow::Result<Vec<Txid>> {
    let mut ws = vec![];
    for c in carried {
        let f: ProofFile = serde_json::from_str(&std::fs::read_to_string(c)?)?;
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
    Ok(Wallet { name: name.into(), ark, ark_w, rgb, online, holdings: vec![] })
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
            let p = state.join(format!("{seq}-track-{who}.arkproof.json"));
            add_vtxos(&w.ark, &regtest, &p, &[v]).await?;
            import(&w.rgb, &w.online, &p, &ops)?;
            own(&w.rgb, v)?;
            w.holdings.push(Holding { asset_id: asset_id.clone(), amount: crate::ISSUE_AMOUNT, kind: "vtxo".into(), outpoint: v.to_string(), proof: None });
            lg.push("ok", format!("{who} holds {} of {asset_id} on VTXO {v}", crate::ISSUE_AMOUNT));
        }
        Action::Send { from, to, asset_id, amount, sats } => {
            let (fi, ti) = (idx(&from)?, idx(&to)?);
            let (f, t) = two_mut(&mut c.wallets, fi, ti)?;
            let h = take_holding(f, &asset_id, &["vtxo"])?;
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
        Action::Refresh { who, asset_id } | Action::Recover { who, asset_id } => {
            let recover = matches!(a, Action::Recover { .. });
            let w = &mut c.wallets[idx(&who)?];
            let h = take_holding(w, &asset_id, &["vtxo"])?;
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
        Action::CollabExit { who, asset_id } => {
            let w = &mut c.wallets[idx(&who)?];
            let h = take_holding(w, &asset_id, &["vtxo"])?;
            let v = OutPoint::from_str(&h.outpoint)?;
            let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
            let exit_addr = Address::from_str(&bl(|| w.rgb.get_address())?)?.assume_checked();
            lg.info(format!("collaborative exit of {v} to {exit_addr} ({EXIT_SAT} sat); the forfeit commits T: {v} -> commitment-tx output"));
            let (commit, fc) = match colored_batch(&who, &w.ark, &w.rgb, cid, v, h.amount, BatchOp::Exit(exit_addr, Amount::from_sat(EXIT_SAT))).await {
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
        Action::UnilateralExit { who, asset_id } => {
            let w = &mut c.wallets[idx(&who)?];
            let i = w.holdings.iter().position(|x| x.asset_id == asset_id && x.kind == "vtxo").ok_or_else(|| anyhow!("no VTXO holding"))?;
            let v = OutPoint::from_str(&w.holdings[i].outpoint)?;
            lg.info(format!("fund {who}'s on-chain wallet for exit fees, then broadcast the branch of {v} (no operator needed)"));
            fund_onchain(&who, &w.ark, &w.ark_w).await?;
            let branch = unilateral_exit(&who, &w.ark, &w.ark_w, v.txid).await?;
            for t in &branch {
                lg.tx("exit branch tx", t);
            }
            let _ = bl(|| w.rgb.update_witnesses(w.online.clone(), 0, vec![]));
            w.holdings[i].kind = "unrolled".into();
            lg.push("ok", format!("{who}: {v} is on-chain; spendable after the exit delay (on-chain send mines blocks until then)"));
        }
        Action::OnchainSend { from, to, asset_id } => {
            let (fi, ti) = (idx(&from)?, idx(&to)?);
            let (f, t) = two_mut(&mut c.wallets, fi, ti)?;
            let h = take_holding(f, &asset_id, &["unrolled", "exit-output"])?;
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
                pay_from_rgb_output(&from, &mut f.rgb, &f.online, &regtest, o, EXIT_SAT, cid, h.amount, script, b, &cons).await
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
            t.holdings.push(Holding { asset_id, amount: h.amount, kind: "onchain".into(), outpoint: format!("{txid}:1"), proof: None });
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
            lg.push("ok", format!("new batches expire after {seconds}s"));
        }
        Action::Sync => {}
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

async fn get_state(State(app): State<AppState>) -> Json<Shared> {
    Json(app.shared.lock().unwrap().clone())
}

async fn post_action(State(app): State<AppState>, Json(a): Json<Action>) -> impl IntoResponse {
    let label = a.label();
    {
        let mut s = app.shared.lock().unwrap();
        if let Some(b) = &s.busy {
            return (StatusCode::CONFLICT, Json(serde_json::json!({"ok": false, "error": format!("busy: {b}")})));
        }
        s.busy = Some(label.clone());
    }
    if app.jobs.lock().unwrap().send(a).is_err() {
        app.shared.lock().unwrap().busy = None;
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok": false, "error": "worker stopped"})));
    }
    (StatusCode::ACCEPTED, Json(serde_json::json!({"ok": true, "started": label})))
}

async fn init(shared: &Arc<StdMutex<Shared>>) -> anyhow::Result<Core> {
    let state = crate::paths::state().join(format!("app-{}", now()));
    std::fs::create_dir_all(&state)?;
    shared.lock().unwrap().state_dir = state.display().to_string();
    let regtest = Arc::new(Regtest::new());
    let lg = Logger { shared: shared.clone(), action: "startup".into() };
    let mut wallets = vec![];
    for n in NAMES {
        wallets.push(new_wallet(n, &regtest, &state).await?);
        lg.info(format!("wallet {n} ready (Ark client + rgb-lib wallet in {})", state.display()));
    }
    let info = wallets[0].ark.server_info().await?;
    shared.lock().unwrap().arkd = serde_json::json!({
        "version": info.version, "signer": info.signer_pk.to_string(), "dust": info.dust.to_sat(),
        "unilateral_exit_delay": format!("{:?}", info.unilateral_exit_delay),
    });
    let core = Core { state, regtest, ops: operators(&info), wallets, seq: 0 };
    refresh_views(&core, shared).await;
    Ok(core)
}

/// The wallets (rgb-lib, Ark client) live on one worker thread with its own runtime; actions are
/// queued to it and run one at a time (the existing flow helpers are not `Send`).
fn worker(shared: Arc<StdMutex<Shared>>, rx: std::sync::mpsc::Receiver<Action>, ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>) {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("runtime");
    rt.block_on(async move {
        let mut core = match init(&shared).await {
            Ok(c) => {
                let _ = ready.send(Ok(()));
                c
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        while let Ok(a) = tokio::task::block_in_place(|| rx.recv()) {
            let label = a.label();
            let lg = Logger { shared: shared.clone(), action: label.clone() };
            lg.push("start", label.clone());
            match run_action(&mut core, &a, &lg).await {
                Ok(()) => lg.push("done", label.clone()),
                Err(e) => lg.push("error", format!("{e:#}")),
            }
            refresh_views(&core, &shared).await;
            shared.lock().unwrap().busy = None;
        }
    });
}

pub async fn run(port: u16) -> anyhow::Result<()> {
    let shared = Arc::new(StdMutex::new(Shared::default()));
    let (tx, rx) = std::sync::mpsc::channel::<Action>();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let sh = shared.clone();
    std::thread::spawn(move || worker(sh, rx, ready_tx));
    let sh = shared.clone();
    std::thread::spawn(move || miner(sh));
    ready_rx.await.map_err(|_| anyhow!("worker died"))??;
    let app = AppState { jobs: Arc::new(StdMutex::new(tx)), shared };
    let router = axum::Router::new()
        .route("/", get(index))
        .route("/api/state", get(get_state))
        .route("/api/action", post(post_action))
        .route("/api/automine", post(post_automine))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("rgb-arkade app on http://127.0.0.1:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}

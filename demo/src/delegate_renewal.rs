//! Scenario `delegate-renewal` (time-scaled, profile `cponly-seconds`: arkd master + B only).
//! Operator-risk options O2 + O1: delegate-key renewal backed by keyless watchtowers.
//! Scale: batch expiry 1024 s = 7 days, so 60 days = 8777 s.
//!
//! * setup: Olivia and Rita board, receive an asset each on a colored delegate template (on-chain
//!   issuance by Ivan + jump-in onto the VTXO), hand the delegate a job (no keys) and go offline.
//!   Negatives on the way: a delegate leaf without S, a delegate spend before T, a delegate theft
//!   after T (arkd accepts it; the receiver-side policy check rejects it), the #961 guard.
//! * the delegate daemon renews both every period; Rita's operator stops cosigning after her
//!   first renewal (the run kills the proxy her delegate talks through): the watchtowers exit.
//! * return olivia (after ~60 days): discovers her VTXO from her keys, validates, sends to Bob.
//! * return rita: the asset is parked on-chain; she spends it to Gina, who validates.
use crate::delegate::{self, Handoff, Kind, Leaf, Template};
use crate::delegated::{Job, job_path, load_job, out_dir};
use crate::forfeit::summary_rec;
use crate::proof::{add_vtxos, build_proof, import, operators, proof_path};
use crate::regtest::Regtest;
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bl, board, bare_opret, color_and_commit, now, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_core::send::SendReceiver;
use bitcoin::{Address, Amount, Network, OutPoint, Psbt, ScriptBuf, Txid};
use rgb_lib::wallet::{Online, Outpoint, RgbWalletOpsOffline, Wallet as RgbWallet};
use rgb_lib::{Assignment, ContractId, FileContent};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const EXPIRY_S: u64 = 1024;
pub(crate) const DAY_S: f64 = EXPIRY_S as f64 / 7.0;
/// the delegate leaf opens this long before expiry (half the batch life, 3.5 days)
pub(crate) const WINDOW_S: u64 = 512;
pub(crate) const TOWER_THRESHOLD_S: i64 = 150;
const COLORED_SAT: u64 = 5_000;
const SPARE_SAT: u64 = 5_000;
const BL_BOB: u64 = 7101;
const BL_GINA: u64 = 7102;
const BL_THEFT: u64 = 7103;

/// Scenario `threshold-delegate`: the same flow with a 2-of-3 delegate (D1 coordinates, D2 and D3
/// run `cosignd`). See [`crate::cosign`].
pub(crate) static THRESHOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn thr() -> bool {
    THRESHOLD.load(std::sync::atomic::Ordering::SeqCst)
}
/// co-signer addresses of D2 and D3 (the run script starts them with these `listen.txt`)
const COSIGN_ADDRS: [&str; 2] = ["127.0.0.1:7091", "127.0.0.1:7092"];
fn cdir(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("cosign-{i}"))
}
fn name() -> &'static str {
    if thr() { "THRESHOLD DELEGATE" } else { "DELEGATE RENEWAL" }
}
fn write_cosign_job(dir: &Path, i: usize, id: &str, base: &Template, asset_id: &str) -> anyhow::Result<()> {
    let j = crate::cosign::CosignJob { id: id.into(), base: base.clone(), asset_id: asset_id.into(), expiry_s: EXPIRY_S, arkd_url: crate::paths::arkd_url() };
    std::fs::create_dir_all(cdir(dir, i).join("jobs"))?;
    std::fs::write(cdir(dir, i).join("jobs").join(format!("{id}.json")), serde_json::to_string_pretty(&j)?)?;
    Ok(())
}

fn users(dir: &Path) -> PathBuf {
    dir.join("users")
}
fn ddir(dir: &Path) -> PathBuf {
    dir.join("delegate")
}
fn towers() -> Vec<PathBuf> {
    vec![crate::paths::watchtowerd().join("A"), crate::paths::watchtowerd().join("B")]
}

/// Ivan issues an NIA on-chain and moves it with an on-chain RGB transfer to the explicit seal
/// `target` (a VTXO of someone else). Returns the asset id and the consignment of the transfer.
async fn issue_to(regtest: &Regtest, ivan: &mut RgbWallet, online: &Online, target: OutPoint, cons: &Path, name: &str) -> anyhow::Result<String> {
    let addr = bl(|| ivan.get_address())?;
    regtest::faucet(&Address::from_str(&addr)?.assume_checked(), "0.001")?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    bl(|| ivan.create_utxos(online.clone(), false, Some(1), Some(5_000), 2, false))?;
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let asset = bl(|| ivan.issue_asset_nia(name.into(), format!("{name} delegate test"), 0, vec![ISSUE_AMOUNT]))?;
    let unspents = bl(|| ivan.list_unspents(Some(online.clone()), false, false))?;
    let colored = unspents
        .iter()
        .find(|u| u.rgb_allocations.iter().any(|a| a.asset_id.as_deref() == Some(asset.asset_id.as_str())))
        .ok_or_else(|| anyhow!("issued allocation not found"))?
        .utxo
        .clone();
    let change = Address::from_str(&bl(|| ivan.get_address())?)?.assume_checked();
    let tx = bitcoin::Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: OutPoint { txid: Txid::from_str(&colored.outpoint.txid)?, vout: colored.outpoint.vout },
            script_sig: ScriptBuf::new(),
            sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: bitcoin::Witness::new(),
        }],
        output: vec![bare_opret(Amount::ZERO), bitcoin::TxOut { value: Amount::from_sat(colored.btc_amount - 500), script_pubkey: change.script_pubkey() }],
    };
    let mut psbt = Psbt::from_unsigned_tx(tx)?;
    psbt.inputs[0] = bl(|| ivan.psbt_input_for_utxo(colored.outpoint.clone()))?;
    let cid = ContractId::from_str(&asset.asset_id).map_err(|e| anyhow!("{e}"))?;
    let transfers = bl(|| ivan.color_psbt_and_consume_to_outpoint(&mut psbt, cid, Outpoint { txid: target.txid.to_string(), vout: target.vout }, 424_242))?;
    transfers.first().ok_or_else(|| anyhow!("no transfer"))?.save_file(cons).map_err(|e| anyhow!("save: {e}"))?;
    let signed = Psbt::from_str(&bl(|| ivan.sign_psbt(psbt.to_string(), None))?)?;
    let t1 = regtest.send_raw(&signed.extract_tx().map_err(|e| anyhow!("{e}"))?)?;
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    println!("[ivan] {name} {} issued and moved on-chain to {target} (tx {t1})", asset.asset_id);
    Ok(asset.asset_id)
}

/// The VTXO of `tpl` created by off-chain tx `txid`.
async fn output_of(net: &ark_grpc::Client, tpl: &Template, txid: Txid) -> anyhow::Result<(OutPoint, i64)> {
    for _ in 0..20 {
        if let Some(v) = delegate::vtxos_at(net, &[tpl.address()]).await?.into_iter().find(|v| v.outpoint.txid == txid) {
            return Ok((v.outpoint, v.expires_at));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    anyhow::bail!("no output of {txid} at the template address")
}

/// Owner receives: register the VTXO from a proof, accept the consignment onto it.
async fn receive_on(client: &ArkClient, regtest: &Regtest, rgb: &mut RgbWallet, online: &Online, info: &ark_core::server::Info, cons: &Path, vtxo: OutPoint) -> anyhow::Result<u64> {
    let pp = proof_path(cons);
    add_vtxos(client, regtest, &pp, &[vtxo]).await?;
    import(rgb, online, &pp, &operators(info))?;
    bl(|| rgb.ark_receive_vtxo_transfer(online.clone(), cons.to_path_buf(), vtxo.to_string())).map_err(|e| anyhow!("{e}"))
}

pub async fn setup(dir: PathBuf) -> anyhow::Result<()> {
    let state = users(&dir);
    std::fs::create_dir_all(&state)?;
    std::fs::create_dir_all(ddir(&dir).join("jobs"))?;
    let mut rec = summary_rec(dir.clone());
    let regtest = Arc::new(Regtest::new());
    let (ivan_ark, _) = ark_client("ivan", regtest.clone(), &state).await?;
    let info = ivan_ark.server_info().await?;
    rec("arkd_version", info.version.clone());
    let net = ivan_ark.network_client();
    let (mut ivan, ivan_online) = bl(|| rgb_wallet("ivan", &state))?;
    let d = delegate::key_at(&ddir(&dir).join("delegate-sk.hex"))?.x_only_public_key().0;
    rec("delegate_key", d.to_string());
    let ds = if thr() {
        let d2 = delegate::key_at(&cdir(&dir, 2).join("key.hex"))?.x_only_public_key().0;
        let d3 = delegate::key_at(&cdir(&dir, 3).join("key.hex"))?.x_only_public_key().0;
        rec("threshold", format!("2-of-3: D1 {d} (coordinator, holds the stash), D2 {d2} and D3 {d3} (cosignd at {COSIGN_ADDRS:?})"));
        vec![d, d2, d3]
    } else {
        vec![]
    };
    let mk = |a: bitcoin::XOnlyPublicKey, kind: Kind| if thr() { Template::threshold(a, &ds, 2, &info, kind) } else { Template::new(a, d, &info, kind) };
    let mut scen = serde_json::Map::new();
    scen.insert("t_setup".into(), now().into());

    step("1. Olivia and Rita board, create a colored delegate template VTXO and a spare, receive an asset");
    for who in ["olivia", "rita"] {
        let (client, _) = ark_client(who, regtest.clone(), &state).await?;
        board(who, &client, "0.001").await?;
        let v = crate::forfeit::first_vtxo(&client).await?;
        let vexp = delegate::vtxo_at(&net, v).await.ok_or_else(|| anyhow!("no info for {v}"))?.expires_at;
        let left = vexp - now() as i64;
        ensure!((800..1300).contains(&left), "batch expiry {left} s, expected ~{EXPIRY_S}: set vtxoTreeExpiry={EXPIRY_S}");
        let a = delegate::save_key(&state.join(format!("{who}-owner-sk.hex")))?.x_only_public_key().0;
        let t0 = ((vexp as u64 - WINDOW_S) / delegate::T_GRID_S * delegate::T_GRID_S) as u32;
        let colored = mk(a, Kind::Colored(t0));
        let spare = mk(a, Kind::Spare);
        let txid = client
            .send(vec![SendReceiver::bitcoin(colored.address(), Amount::from_sat(COLORED_SAT)), SendReceiver::bitcoin(spare.address(), Amount::from_sat(SPARE_SAT))])
            .await
            .map_err(|e| anyhow!("{who}: send to templates: {e:#}"))?;
        let (c0, cexp) = output_of(&net, &colored, txid).await?;
        let (p0, _) = output_of(&net, &spare, txid).await?;
        rec(&format!("{who}_templates"), format!("colored {c0} (CLTV T {t0}, opens {} s before expiry {cexp}), spare {p0}; leaves: {{S,A}}, CSV(A), {} delegate leaf(s)", cexp - t0 as i64, colored.delegate_leaves().len()));
        let cons = state.join(format!("{who}-receive.rgbc"));
        let asset_id = issue_to(&regtest, &mut ivan, &ivan_online, c0, &cons, &who.to_uppercase()[..4]).await?;
        let (mut rgb, online) = bl(|| rgb_wallet(who, &state))?;
        let got = receive_on(&client, &regtest, &mut rgb, &online, &info, &cons, c0).await?;
        rec(&format!("{who}_received"), format!("{got} units of {asset_id} on {c0} (validated: on-chain jump-in + Ark proof of the VTXO)"));
        ensure!(got == ISSUE_AMOUNT, "{who} received {got}");
        let url = if who == "rita" && !thr() { std::env::var("RGB_ARKADE_DELEGATE_PROXY_URL").unwrap_or_else(|_| "http://localhost:7080".into()) } else { crate::paths::arkd_url() };
        let job = Job {
            id: who.into(),
            arkd_url: url.clone(),
            asset_id: asset_id.clone(),
            units: ISSUE_AMOUNT,
            colored: c0.to_string(),
            colored_template: Some(colored.clone()),
            colored_sat: COLORED_SAT,
            colored_expires_at: cexp,
            spare: p0.to_string(),
            spare_template: Some(spare.clone()),
            spare_sat: SPARE_SAT,
            consignment: cons.display().to_string(),
            proof: proof_path(&cons).display().to_string(),
            witnesses: vec![],
            towers: towers(),
            t_margin_s: 20,
            window_s: WINDOW_S,
            expiry_s: EXPIRY_S,
            tower_threshold_s: TOWER_THRESHOLD_S,
            t_offline: now(),
            state: "new".into(),
            cosigners: thr().then(|| crate::cosign::Cosigners { addrs: vec![String::new(), COSIGN_ADDRS[0].into(), COSIGN_ADDRS[1].into()] }),
            me: 0,
            ..Default::default()
        };
        if thr() {
            for i in [2, 3] {
                write_cosign_job(&dir, i, who, &spare, &asset_id)?;
            }
        }
        let text = serde_json::to_string_pretty(&job)?;
        let leaks = [std::fs::read_to_string(state.join(format!("{who}-owner-sk.hex")))?, std::fs::read_to_string(state.join(format!("{who}-rgb-mnemonic.txt")))?];
        ensure!(leaks.iter().all(|s| !text.contains(s.trim())), "secret in the job");
        std::fs::write(job_path(&ddir(&dir), who), text)?;
        rec(&format!("{who}_job"), format!("delegate via {url}; job has no owner secret; towers {:?}", towers()));
        let mut u = serde_json::Map::new();
        u.insert("asset_id".into(), asset_id.into());
        u.insert("t_offline".into(), now().into());
        u.insert("c0".into(), c0.to_string().into());
        scen.insert(who.into(), u.into());
        std::fs::write(dir.join("scenario.json"), serde_json::to_string_pretty(&scen)?)?;
    }

    if thr() {
        threshold_negatives(&dir, &state, &regtest, &net, &info, &mut ivan, &ivan_online, &ds, &mut rec).await?;
    } else {
        step("2. negatives with a separate (malicious) delegate M_D and owner Nora");
        let mal_dir = state.join("mal");
        std::fs::create_dir_all(&mal_dir)?;
        let md = delegate::save_key(&mal_dir.join("delegate-sk.hex"))?;
        let (mal_ark, _) = ark_client("mal", regtest.clone(), &mal_dir).await?;
        board("mal", &mal_ark, "0.0005").await?;
        let (mal_addr, _) = mal_ark.get_offchain_address().await?;
        let (mut mal_rgb, mal_online) = bl(|| rgb_wallet("mal", &mal_dir))?;
        let (nora, _) = ark_client("nora", regtest.clone(), &state).await?;
        board("nora", &nora, "0.001").await?;
        let na = delegate::save_key(&state.join("nora-owner-sk.hex"))?;
        let n_a = na.x_only_public_key().0;
        let n_v = crate::forfeit::first_vtxo(&nora).await?;
        let n_exp = delegate::vtxo_at(&net, n_v).await.map(|x| x.expires_at).unwrap_or(0);

        // N1: a delegate leaf without the operator key
        let bad = Template::new(n_a, md.x_only_public_key().0, &info, Kind::NoSigner);
        let bt = nora.send(vec![SendReceiver::bitcoin(bad.address(), Amount::from_sat(2_000))]).await.map_err(|e| anyhow!("fund bad: {e:#}"))?;
        let (bo, _) = output_of(&net, &bad, bt).await?;
        let r = delegate::submit_hop(&net, &info, bad.send_input(bo, Amount::from_sat(2_000), Leaf::Delegate), &[SendReceiver::bitcoin(mal_addr, Amount::from_sat(2_000))], &mal_addr, std::slice::from_ref(&md), None, None).await;
        rec("neg1_delegate_leaf_without_S", format!("{:?}", r.as_ref().map_err(|e| e.to_string().chars().take(260).collect::<String>())));
        ensure!(r.is_err(), "arkd accepted a spend through a leaf without the operator key");
        let r2 = delegate::submit_hop(&net, &info, bad.send_input(bo, Amount::from_sat(2_000), Leaf::Owner), &[SendReceiver::bitcoin(mal_addr, Amount::from_sat(2_000))], &mal_addr, std::slice::from_ref(&na), None, None).await;
        rec("neg1b_owner_spend_of_that_vtxo", format!("{:?} (arkd validates the whole input taptree: a VTXO with a leaf that skips S can only be exited)", r2.as_ref().map_err(|e| e.to_string().chars().take(200).collect::<String>())));

        // N2: delegate spend before T (to the delegate itself)
        let t_future = ((n_exp as u64 - WINDOW_S) / delegate::T_GRID_S * delegate::T_GRID_S) as u32;
        let n_col = Template::new(n_a, md.x_only_public_key().0, &info, Kind::Colored(t_future));
        let n_past = Template::new(n_a, md.x_only_public_key().0, &info, Kind::Colored(((now() - 2 * delegate::T_GRID_S) / delegate::T_GRID_S * delegate::T_GRID_S) as u32));
        let nt = nora
            .send(vec![SendReceiver::bitcoin(n_col.address(), Amount::from_sat(COLORED_SAT)), SendReceiver::bitcoin(n_past.address(), Amount::from_sat(COLORED_SAT))])
            .await
            .map_err(|e| anyhow!("fund nora: {e:#}"))?;
        let (n1, _) = output_of(&net, &n_col, nt).await?;
        let (n2, _) = output_of(&net, &n_past, nt).await?;
        let (mut nora_rgb, nora_online) = bl(|| rgb_wallet("nora", &state))?;
        let mut nassets = vec![];
        for (i, o) in [n1, n2].into_iter().enumerate() {
            let cons = state.join(format!("nora-receive-{i}.rgbc"));
            let aid = issue_to(&regtest, &mut ivan, &ivan_online, o, &cons, &format!("NOR{i}")).await?;
            ensure!(receive_on(&nora, &regtest, &mut nora_rgb, &nora_online, &info, &cons, o).await? == ISSUE_AMOUNT, "nora receive");
            // Nora hands M_D the stash, as an owner does with a delegate
            import(&mal_rgb, &mal_online, &proof_path(&cons), &operators(&info))?;
            ensure!(bl(|| mal_rgb.ark_receive_vtxo_transfer(mal_online.clone(), cons.clone(), o.to_string())).map_err(|e| anyhow!("{e}"))? == ISSUE_AMOUNT, "mal stash");
            nassets.push((aid, cons));
        }
        let (mal_self, _) = (mal_ark.get_offchain_address().await?, ());
        let mal_vtxo = crate::forfeit::first_vtxo(&mal_ark).await?;
        let cid1 = ContractId::from_str(&nassets[0].0).map_err(|e| anyhow!("{e}"))?;
        let r = delegate::rgb_hop(&net, &info, &mal_rgb, &mal_online, n_col.send_input(n1, Amount::from_sat(COLORED_SAT), Leaf::Delegate), &[SendReceiver::bitcoin(mal_self.0, Amount::from_sat(COLORED_SAT))], &mal_self.0, std::slice::from_ref(&md), cid1, vec![(mal_vtxo, ISSUE_AMOUNT, BL_THEFT)], None).await;
        rec("neg2_delegate_before_T_to_itself", format!("T {t_future} is {} s ahead: {:?}", t_future as i64 - now() as i64, r.as_ref().map(|x| x.0).map_err(|e| e.to_string().chars().take(260).collect::<String>())));
        ensure!(r.is_err(), "arkd accepted a delegate spend before its CLTV");

        // N3: once T has passed, D + S can move the asset anywhere: a theft to Mallory
        let (mallory, _) = ark_client("mallory", regtest.clone(), &state).await?;
        board("mallory", &mallory, "0.0005").await?;
        let m = crate::forfeit::first_vtxo(&mallory).await?;
        delegate::wait_commitments(&net, &regtest, m).await?;
        let cid2 = ContractId::from_str(&nassets[1].0).map_err(|e| anyhow!("{e}"))?;
        let (tcp, _tark, ttr) = delegate::rgb_hop(&net, &info, &mal_rgb, &mal_online, n_past.send_input(n2, Amount::from_sat(COLORED_SAT), Leaf::Delegate), &[SendReceiver::bitcoin(mal_self.0, Amount::from_sat(COLORED_SAT))], &mal_self.0, std::slice::from_ref(&md), cid2, vec![(m, ISSUE_AMOUNT, BL_THEFT + 1)], None)
            .await
            .map_err(|e| anyhow!("theft after T was refused: {e:#}"))?;
        rec("neg3_delegate_after_T_theft_accepted_by_arkd", format!("checkpoint {tcp}: arkd cosigned D's spend to Mallory's {m} (the protocol does not prevent it)"));
        let tcons = state.join("theft-to-mallory.rgbc");
        ttr.save_file(&tcons).map_err(|e| anyhow!("{e}"))?;
        let tpp = proof_path(&tcons);
        build_proof(&mallory, &regtest, &[tcp], &[proof_path(&nassets[1].1)], &tpp).await?;
        add_vtxos(&mallory, &regtest, &tpp, &[m]).await?;
        let (mut mal_recv, mro) = bl(|| rgb_wallet("mallory", &state))?;
        import(&mal_recv, &mro, &tpp, &operators(&info))?;
        let got = bl(|| mal_recv.ark_receive_vtxo_transfer(mro.clone(), tcons.clone(), m.to_string())).map_err(|e| anyhow!("{e}"));
        rec("neg3_stolen_asset_valid_rgb", format!("plain RGB + Ark proof: {:?}", got.as_ref().map_err(|e| e.to_string())));
        let rep = delegate::policy_check(&regtest, &tcons, &tpp, &[n_col.clone(), n_past.clone()], delegate::server_xonly(&info))?;
        rec("neg3_policy_check", format!("delegate spends {}, violations {:?}", rep.delegate_spends, rep.violations));
        ensure!(!rep.violations.is_empty(), "the policy check accepted a delegate theft");
    }

    // N4: #961 guard on Olivia's colored VTXO (off-chain now; the on-chain case is checked by the daemon and on Rita's return)
    let oj = load_job(&ddir(&dir), "olivia")?;
    let oc = OutPoint::from_str(&oj.colored)?;
    let colored: HashSet<OutPoint> = [oc].into();
    let probe = vec![oj.colored_template.clone().unwrap().intent_input(oc, Amount::from_sat(COLORED_SAT), Leaf::Owner, true)];
    let g = delegate::guard_batch_inputs(&probe, &colored, None);
    rec("neg4_guard_961", format!("{:?}", g.as_ref().map_err(|e| e.to_string())));
    ensure!(g.is_err(), "guard let a colored VTXO into a batch");

    rec("setup_done_at", now().to_string());
    println!("\n{} SETUP RESULT: PASS (owners offline now)", name());
    Ok(())
}

/// [`import`], retried: the Electrum resolver sometimes misses a tx for a few seconds.
async fn import_retry(rgb: &RgbWallet, online: &Online, proof: &Path, ops: &str) -> anyhow::Result<()> {
    let mut last = None;
    for i in 0..6 {
        match import(rgb, online, proof, ops) {
            Ok(_) => return Ok(()),
            Err(e) => {
                println!("[import] attempt {i}: {e:#}");
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    Err(last.unwrap())
}

fn load_handoff(dir: &Path, who: &str) -> anyhow::Result<Handoff> {
    Ok(serde_json::from_str(&std::fs::read_to_string(out_dir(&ddir(dir), who).join("handoff.json"))?)?)
}

pub async fn return_user(who: &str, dir: PathBuf) -> anyhow::Result<()> {
    let state = users(&dir);
    let rdir = dir.join(format!("return-{who}"));
    std::fs::create_dir_all(&rdir)?;
    let mut rec = summary_rec(rdir.clone());
    let regtest = Arc::new(Regtest::new());
    let scen: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("scenario.json"))?)?;
    let t_off = scen[who]["t_offline"].as_u64().unwrap_or(0);
    rec("days_offline", format!("{:.1}", (now() - t_off) as f64 / DAY_S));
    let (client, _) = crate::common::ark_client_load(who, regtest.clone(), &state).await?;
    let info = client.server_info().await?;
    let net = client.network_client();
    let a = delegate::load_key(&state.join(format!("{who}-owner-sk.hex")))?;
    let job = load_job(&ddir(&dir), who)?;
    rec("delegate_job", format!("state {}, renewals {}, failures {}", job.state, job.renewals, job.failures));
    let refused: Vec<&String> = job.events.iter().filter(|e| e.contains("REFUSED")).collect();
    for e in job.events.iter().filter(|e| e.contains("ON-CHAIN") || e.contains("done in")) {
        println!("  delegate: {e}");
    }
    if let (Some(f), Some(l)) = (refused.first(), refused.last()) {
        println!("  delegate: {} failed renewal attempts, first: {f}\n  delegate: last: {l}", refused.len());
    }
    let h = load_handoff(&dir, who).unwrap_or_else(|_| Handoff {
        asset_id: job.asset_id.clone(),
        current: job.colored.clone(),
        current_template: job.colored_template.clone(),
        consignment: job.consignment.clone(),
        proof: job.proof.clone(),
        templates: job.templates.clone(),
        witnesses: job.witnesses.clone(),
    });
    let cur = OutPoint::from_str(&h.current)?;
    let tpl = h.current_template.clone().ok_or_else(|| anyhow!("no template"))?;
    let (mut rgb, online) = bl(|| crate::common::rgb_wallet_load(who, &state))?;
    let cid = ContractId::from_str(&h.asset_id).map_err(|e| anyhow!("{e}"))?;
    let ops = operators(&info);
    let server = delegate::server_xonly(&info);

    if who == "olivia" {
        step("olivia: find the renewed VTXO from her own keys (CLTV grid scan), then validate the delegate's proof");
        let base = tpl.with_kind(Kind::Colored(0));
        let (found, scanned) = delegate::discover(&net, &base, t_off, now() + EXPIRY_S).await?;
        rec("discovery", format!("{scanned} grid addresses scanned, live colored VTXOs {:?}", found.iter().map(|(v, t)| format!("{} (T {:?})", v.outpoint, t.cltv())).collect::<Vec<_>>()));
        let (fv, ftpl) = found.iter().find(|(v, _)| v.outpoint == cur).cloned().ok_or_else(|| anyhow!("handoff VTXO {cur} not found by the scan"))?;
        ensure!(ftpl == tpl, "template mismatch");
        rec("current_vtxo", format!("{cur}: {} s left before expiry ({:.1} days), {} renewals", fv.expires_at - now() as i64, (fv.expires_at - now() as i64) as f64 / DAY_S, job.renewals));
        let ti = Instant::now();
        import_retry(&rgb, &online, Path::new(&h.proof), &ops).await?;
        let imp = ti.elapsed().as_millis();
        let tp = Instant::now();
        let rep = delegate::policy_check(&regtest, Path::new(&h.consignment), Path::new(&h.proof), &h.templates, server)?;
        rec("policy_check", format!("{} witnesses, {} delegate spends, violations {:?} ({} ms)", rep.witnesses, rep.delegate_spends, rep.violations, tp.elapsed().as_millis()));
        ensure!(rep.violations.is_empty() && rep.delegate_spends as u32 == job.renewals, "policy check failed");
        let tr = Instant::now();
        // the Electrum resolver sometimes misses an old tx right after a block: retry (as for the import)
        let mut got = Err(anyhow!("not tried"));
        for i in 0..6 {
            got = bl(|| rgb.ark_receive_vtxo_transfer(online.clone(), PathBuf::from(&h.consignment), cur.to_string())).map_err(|e| anyhow!("{e}"));
            match &got {
                Err(e) if e.to_string().contains("resolver") => {
                    println!("[validate] attempt {i}: {e:#}");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
                _ => break,
            }
        }
        let got = got?;
        rec("olivia_validated", format!("{got} units on {cur}; proof {} B, import {imp} ms, validate+accept {} ms", crate::proof::file_len(Path::new(&h.proof)), tr.elapsed().as_millis()));
        ensure!(got == ISSUE_AMOUNT, "olivia got {got}");

        step("olivia sends the asset to Bob (owner leaf {S,A}), Bob validates");
        let (bob, _) = ark_client("bob", regtest.clone(), &rdir).await?;
        board("bob", &bob, "0.0005").await?;
        let hb = crate::forfeit::first_vtxo(&bob).await?;
        delegate::wait_commitments(&net, &regtest, hb).await?;
        let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &rdir))?;
        let spare = tpl.with_kind(Kind::Spare);
        let (cp, ark, tr) = delegate::rgb_hop(&net, &info, &rgb, &online, tpl.send_input(cur, fv.amount, Leaf::Owner), &[SendReceiver::bitcoin(spare.address(), fv.amount)], &spare.address(), std::slice::from_ref(&a), cid, vec![(hb, ISSUE_AMOUNT, BL_BOB)], None).await?;
        rec("olivia_to_bob", format!("checkpoint {cp} ark tx {ark}: {ISSUE_AMOUNT} units -> Bob's {hb}"));
        let bcons = rdir.join("olivia-to-bob.rgbc");
        tr.save_file(&bcons).map_err(|e| anyhow!("{e}"))?;
        let plain = bl(|| bob_rgb.validate_consignment_ark(bob_online.clone(), bcons.clone(), false));
        rec("bob_plain_rgb_without_proof", format!("{:?}", plain.as_ref().map_err(|e| e.to_string())));
        let bpp = proof_path(&bcons);
        let mut wit: Vec<Txid> = h.witnesses.iter().filter_map(|w| Txid::from_str(w).ok()).collect();
        wit.push(cp);
        let st = build_proof(&client, &regtest, &wit, &[PathBuf::from(&h.proof)], &bpp).await?;
        add_vtxos(&bob, &regtest, &bpp, &[hb]).await?;
        let ti = Instant::now();
        import_retry(&bob_rgb, &bob_online, &bpp, &ops).await?;
        let imp = ti.elapsed().as_millis();
        let rep = delegate::policy_check(&regtest, &bcons, &bpp, &h.templates, server)?;
        rec("bob_policy_check", format!("{} delegate spends, violations {:?}", rep.delegate_spends, rep.violations));
        ensure!(rep.violations.is_empty(), "Bob's policy check failed");
        let tr = Instant::now();
        let got = bl(|| bob_rgb.ark_receive_vtxo_transfer(bob_online.clone(), bcons.clone(), hb.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec(
            "bob_received",
            format!("{got} units on {hb}; proof {} B ({} gz), {} witnesses, {} PSBTs; consignment {} B ({} gz); import {imp} ms, validate+accept {} ms", crate::proof::file_len(&bpp), delegate::gz_len(&bpp), st.n_witnesses, st.n_psbts, crate::proof::file_len(&bcons), delegate::gz_len(&bcons), tr.elapsed().as_millis()),
        );
        ensure!(got == ISSUE_AMOUNT, "Bob got {got}");
        println!("{} RETURN OLIVIA RESULT: PASS", name());
    } else {
        let gcons = rdir.join("rita-to-gina.rgbc");
        let txid_file = rdir.join("rita-to-gina.txid");
        let resumed = txid_file.exists() && gcons.exists();
        if !resumed {
        step("rita: the operator stopped cosigning her delegate; the watchtowers put the asset on-chain");
        ensure!(regtest.tx_confirmations(&cur.txid).is_some(), "{cur} is not on-chain (state {})", job.state);
        let towers: Vec<String> = towers()
            .iter()
            .filter_map(|w| crate::watchtowerd::read_status(w, &format!("rita-{}", job.renewals)).map(|s| format!("{}: {} {:?}", w.file_name().unwrap().to_string_lossy(), s.state, s.broadcast)))
            .collect();
        rec("towers", format!("{towers:?}"));
        rec("onchain", format!("{cur} confirmed ({:?} confs), unspent: {}", regtest.tx_confirmations(&cur.txid), crate::exit_watchtower::find_spender(&regtest, cur, 2000)?.is_none()));
        import_retry(&rgb, &online, Path::new(&h.proof), &ops).await?;
        let got = bl(|| rgb.ark_receive_vtxo_transfer(online.clone(), PathBuf::from(&h.consignment), cur.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("rita_validated", format!("{got} units on the exited output {cur}; status {:?}", bl(|| rgb.ark_vtxo_status(online.clone(), cur.to_string())).map_err(|e| e.to_string())));
        ensure!(got == ISSUE_AMOUNT, "rita got {got}");
        let probe = vec![tpl.intent_input(cur, Amount::from_sat(job.colored_sat), Leaf::Owner, true)];
        let g = delegate::guard_batch_inputs(&probe, &HashSet::new(), Some((&rgb, &online)));
        rec("guard_961_onchain", format!("re-board the exited colored output as a batch input: {:?}", g.as_ref().map_err(|e| e.to_string())));
        ensure!(g.is_err(), "guard let an on-chain colored VTXO into a batch");
        let refused = job.events.iter().filter(|e| e.contains("REFUSED")).count();
        rec("delegate_refusals", format!("{refused} renewal attempts failed after {}; job state {}", if thr() { "2 of 3 delegates went offline" } else { "the operator stopped cosigning" }, job.state));

        step("rita spends the exited output on-chain (CSV exit leaf) to Gina; Gina validates");
        }
        let (mut gina, gina_online) = if rdir.join("gina-rgb-mnemonic.txt").exists() { bl(|| crate::common::rgb_wallet_load("gina", &rdir))? } else { bl(|| rgb_wallet("gina", &rdir))? };
        let txid = if resumed {
            println!("[rita] resuming: spend to Gina already broadcast");
            Txid::from_str(std::fs::read_to_string(&txid_file)?.trim())?
        } else {
        let recv = bl(|| gina.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
        let gaddr = Address::from_script(&rgb_lib::utils::script_buf_from_recipient_id(recv.recipient_id.clone())?.ok_or_else(|| anyhow!("not witness"))?, Network::Regtest)?;
        let change = client.get_onchain_address()?;
        let sat = regtest.raw_tx(&cur.txid).and_then(|t| t.output.get(cur.vout as usize).map(|o| o.value)).ok_or_else(|| anyhow!("exited output"))?;
        let mut txid = None;
        for attempt in 0..90 {
            let mut transfers = None;
            let slot = &mut transfers;
            let hook = |p: &mut Psbt| -> Result<(), ark_core::Error> {
                let (t, _) = color_and_commit(&rgb, p, cid, 1, ISSUE_AMOUNT, BL_GINA).map_err(|e| ark_core::Error::ad_hoc(format!("{e:#}")))?;
                *slot = Some(t);
                Ok(())
            };
            let tx = ark_core::unilateral_exit::create_unilateral_exit_transaction_with_hook(
                gaddr.clone(),
                Amount::from_sat(2_000),
                change.clone(),
                &[],
                &[tpl.exit_input(cur, sat)],
                vec![bare_opret(Amount::ZERO)],
                Amount::from_sat(600),
                hook,
                delegate::signer(a),
            )?;
            match regtest.send_raw(&tx) {
                Ok(t) => {
                    transfers.ok_or_else(|| anyhow!("no transfer"))?[0].save_file(&gcons).map_err(|e| anyhow!("{e}"))?;
                    txid = Some(t);
                    break;
                }
                Err(e) => {
                    let _ = bl(|| rgb.ark_rollback_transfer(online.clone(), tx.compute_txid().to_string()));
                    if attempt % 6 == 0 {
                        println!("[rita] exit spend not final yet (attempt {attempt}): {e}");
                    }
                    regtest::mine(1)?;
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
        let txid = txid.ok_or_else(|| anyhow!("rita could not spend the exited output"))?;
        std::fs::write(&txid_file, txid.to_string())?;
        regtest::mine(1)?;
        txid
        };
        rec("rita_to_gina", txid.to_string());
        let plain = bl(|| gina.validate_consignment_ark(gina_online.clone(), gcons.clone(), false));
        rec("gina_plain_rgb_without_proof", format!("{:?}", plain.as_ref().map_err(|e| e.to_string())));
        let gpp = proof_path(&gcons);
        let wit: Vec<Txid> = h.witnesses.iter().filter_map(|w| Txid::from_str(w).ok()).collect();
        build_proof(&client, &regtest, &wit, &[PathBuf::from(&h.proof)], &gpp).await?;
        import_retry(&gina, &gina_online, &gpp, &ops).await?;
        let rep = delegate::policy_check(&regtest, &gcons, &gpp, &h.templates, server)?;
        rec("gina_policy_check", format!("{} delegate spends, violations {:?}", rep.delegate_spends, rep.violations));
        ensure!(rep.violations.is_empty(), "Gina's policy check failed");
        crate::checkpoint_offline::settle(&mut gina, &gina_online, &gcons, &h.asset_id, &mut rec).await?;
        println!("{} RETURN RITA RESULT: PASS", name());
    }
    Ok(())
}

/// Print the per-renewal metrics of every job.
pub async fn report(dir: PathBuf) -> anyhow::Result<()> {
    for who in ["olivia", "rita"] {
        let p = out_dir(&ddir(&dir), who).join("metrics.jsonl");
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        println!("\n{who}: renewal | day | proof B | gz B | PSBTs | consignment B | gz B | import ms | validate ms | policy ms | renewal s | subset | cosign calls | cosign ms");
        for l in text.lines() {
            let m: crate::delegated::Metric = serde_json::from_str(l)?;
            println!(
                "  {:>2} | {:>5.1} | {:>6} | {:>5} | {:>3} | {:>6} | {:>5} | {:>4} | {:>4} | {:>3} | {:>4.1} | {:03b} | {:>2} | {:>5}",
                m.renewal, m.days_offline, m.proof_bytes, m.proof_gz_bytes, m.proof_psbts, m.consignment_bytes, m.consignment_gz_bytes, m.import_ms, m.receive_validate_ms, m.policy_ms, m.renewal_s, m.subset, m.cosign_calls, m.cosign_ms
            );
        }
    }
    println!("{} REPORT RESULT: PASS", name());
    Ok(())
}

/// Threshold negatives with owner Nora, a malicious coordinator M_D (holds Nora's stash) and the
/// honest D2/D3 co-signers of this run; and a second template where M_D colludes with M_C.
#[allow(clippy::too_many_arguments)]
async fn threshold_negatives(
    dir: &Path,
    state: &Path,
    regtest: &Arc<Regtest>,
    net: &ark_grpc::Client,
    info: &ark_core::server::Info,
    ivan: &mut RgbWallet,
    ivan_online: &Online,
    ds: &[bitcoin::XOnlyPublicKey],
    rec: &mut impl FnMut(&str, String),
) -> anyhow::Result<()> {
    step("2. threshold negatives: malicious coordinator M_D (with honest D2, D3) and colluding M_D + M_C");
    let mal_dir = state.join("mal");
    std::fs::create_dir_all(&mal_dir)?;
    let md = delegate::save_key(&mal_dir.join("delegate-sk.hex"))?;
    let mc = delegate::save_key(&mal_dir.join("colluder-sk.hex"))?;
    let (mal_ark, _) = ark_client("mal", regtest.clone(), &mal_dir).await?;
    board("mal", &mal_ark, "0.0005").await?;
    let (mut mal_rgb, mal_online) = bl(|| rgb_wallet("mal", &mal_dir))?;
    let (nora, _) = ark_client("nora", regtest.clone(), state).await?;
    board("nora", &nora, "0.001").await?;
    let n_a = delegate::save_key(&state.join("nora-owner-sk.hex"))?.x_only_public_key().0;
    let t_past = ((now() - 2 * delegate::T_GRID_S) / delegate::T_GRID_S * delegate::T_GRID_S) as u32;
    // honest set {M_D, D2, D3}; colluding set {M_D, M_C, D3}
    let hon = Template::threshold(n_a, &[md.x_only_public_key().0, ds[1], ds[2]], 2, info, Kind::Colored(t_past));
    let col = Template::threshold(n_a, &[md.x_only_public_key().0, mc.x_only_public_key().0, ds[2]], 2, info, Kind::Colored(t_past));
    let nt = nora
        .send(vec![SendReceiver::bitcoin(hon.address(), Amount::from_sat(COLORED_SAT)), SendReceiver::bitcoin(col.address(), Amount::from_sat(COLORED_SAT))])
        .await
        .map_err(|e| anyhow!("fund nora: {e:#}"))?;
    let (n1, _) = output_of(net, &hon, nt).await?;
    let (n2, _) = output_of(net, &col, nt).await?;
    let (mut nora_rgb, nora_online) = bl(|| rgb_wallet("nora", state))?;
    let mut nassets = vec![];
    for (i, o) in [n1, n2].into_iter().enumerate() {
        let cons = state.join(format!("nora-receive-{i}.rgbc"));
        let aid = issue_to(regtest, ivan, ivan_online, o, &cons, &format!("NOR{i}")).await?;
        ensure!(receive_on(&nora, regtest, &mut nora_rgb, &nora_online, info, &cons, o).await? == ISSUE_AMOUNT, "nora receive");
        import(&mal_rgb, &mal_online, &proof_path(&cons), &operators(info))?;
        ensure!(bl(|| mal_rgb.ark_receive_vtxo_transfer(mal_online.clone(), cons.clone(), o.to_string())).map_err(|e| anyhow!("{e}"))? == ISSUE_AMOUNT, "mal stash");
        nassets.push((aid, cons));
    }
    write_cosign_job(dir, 2, "nora", &hon.with_kind(Kind::Spare), &nassets[0].0)?;
    let (mallory, _) = ark_client("mallory", regtest.clone(), state).await?;
    board("mallory", &mallory, "0.0005").await?;
    let m = crate::forfeit::first_vtxo(&mallory).await?;
    delegate::wait_commitments(net, regtest, m).await?;
    let (mal_self, _) = mal_ark.get_offchain_address().await?;
    let cid1 = ContractId::from_str(&nassets[0].0).map_err(|e| anyhow!("{e}"))?;
    let leaf12 = Leaf::Subset(0b011);

    let spent = std::cell::Cell::new(Duration::ZERO);
    // N3t-c: the subtle theft: the bitcoin goes back to Nora's own spare template (passes the
    // ark-output check) but the RGB seal points at Mallory's VTXO. D2 must refuse it on the seal.
    let nspare = hon.with_kind(Kind::Spare).address();
    let r = delegate::rgb_hop(net, info, &mal_rgb, &mal_online, hon.send_input(n1, Amount::from_sat(COLORED_SAT), leaf12), &[SendReceiver::bitcoin(nspare.clone(), Amount::from_sat(COLORED_SAT))], &nspare, std::slice::from_ref(&md), cid1, vec![(m, ISSUE_AMOUNT, BL_THEFT + 3)], Some(("nora", &[COSIGN_ADDRS[0].to_string()], &spent))).await;
    rec("neg3t_c_seal_to_mallory_cosign_request_to_honest_D2", format!("{:?}", r.as_ref().map(|x| x.0).map_err(|e| e.to_string().chars().take(300).collect::<String>())));
    ensure!(r.is_err(), "honest co-signer signed a seal theft");

    // N3t-b: M_D asks the honest D2 to co-sign the theft: D2's own checks refuse it
    let r = delegate::rgb_hop(net, info, &mal_rgb, &mal_online, hon.send_input(n1, Amount::from_sat(COLORED_SAT), leaf12), &[SendReceiver::bitcoin(mal_self, Amount::from_sat(COLORED_SAT))], &mal_self, std::slice::from_ref(&md), cid1, vec![(m, ISSUE_AMOUNT, BL_THEFT)], Some(("nora", &[COSIGN_ADDRS[0].to_string()], &spent))).await;
    rec("neg3t_b_theft_cosign_request_to_honest_D2", format!("{:?}", r.as_ref().map(|x| x.0).map_err(|e| e.to_string().chars().take(300).collect::<String>())));
    ensure!(r.is_err(), "honest co-signer signed a theft");

    // N3t-a: M_D alone (its own signature only, no co-signer) after T
    let r = delegate::rgb_hop(net, info, &mal_rgb, &mal_online, hon.send_input(n1, Amount::from_sat(COLORED_SAT), leaf12), &[SendReceiver::bitcoin(mal_self, Amount::from_sat(COLORED_SAT))], &mal_self, std::slice::from_ref(&md), cid1, vec![(m, ISSUE_AMOUNT, BL_THEFT + 2)], None).await;
    rec("neg3t_a_single_delegate_after_T", format!("{:?}", r.as_ref().map(|x| x.0).map_err(|e| e.to_string().chars().take(300).collect::<String>())));
    ensure!(r.is_err(), "arkd accepted a threshold-leaf spend signed by one delegate");
    let st = delegate::vtxo_at(net, n1).await.map(|v| format!("spent {} swept {} preconfirmed {}", v.is_spent, v.is_swept, v.is_preconfirmed)).unwrap_or_default();
    rec("neg3t_a_vtxo_after_attempt", format!("{n1}: {st} (a refused single-delegate attempt must not lock the VTXO)"));
    // the honest owner can still move it (owner leaf {S,A}): proves no pending lock
    let na = delegate::load_key(&state.join("nora-owner-sk.hex"))?;
    let (nself, _) = nora.get_offchain_address().await?;
    let r = delegate::submit_hop(net, info, hon.send_input(n1, Amount::from_sat(COLORED_SAT), Leaf::Owner), &[SendReceiver::bitcoin(nself, Amount::from_sat(COLORED_SAT))], &nself, std::slice::from_ref(&na), None, None).await;
    rec("neg3t_a_owner_spend_after_attempt", format!("{:?} (bitcoin only, the asset is not carried: test of the lock, not a transfer)", r.as_ref().map(|x| x.0).map_err(|e| e.to_string().chars().take(200).collect::<String>())));

    // N3c: two colluding delegates (M_D + M_C) can move the asset: residual trust
    let cid2 = ContractId::from_str(&nassets[1].0).map_err(|e| anyhow!("{e}"))?;
    let (tcp, _tark, ttr) = delegate::rgb_hop(net, info, &mal_rgb, &mal_online, col.send_input(n2, Amount::from_sat(COLORED_SAT), leaf12), &[SendReceiver::bitcoin(mal_self, Amount::from_sat(COLORED_SAT))], &mal_self, &[md, mc], cid2, vec![(m, ISSUE_AMOUNT, BL_THEFT + 1)], None)
        .await
        .map_err(|e| anyhow!("collusion after T was refused: {e:#}"))?;
    rec("neg3c_two_colluding_delegates_accepted_by_arkd", format!("checkpoint {tcp}: M_D + M_C (2 of 3) moved Nora's asset to Mallory's {m}"));
    let tcons = state.join("theft-to-mallory.rgbc");
    ttr.save_file(&tcons).map_err(|e| anyhow!("{e}"))?;
    let tpp = proof_path(&tcons);
    build_proof(&mallory, regtest, &[tcp], &[proof_path(&nassets[1].1)], &tpp).await?;
    add_vtxos(&mallory, regtest, &tpp, &[m]).await?;
    let (mut mal_recv, mro) = bl(|| rgb_wallet("mallory", state))?;
    import(&mal_recv, &mro, &tpp, &operators(info))?;
    let got = bl(|| mal_recv.ark_receive_vtxo_transfer(mro.clone(), tcons.clone(), m.to_string())).map_err(|e| anyhow!("{e}"));
    rec("neg3c_stolen_asset_valid_rgb", format!("plain RGB + Ark proof: {:?}", got.as_ref().map_err(|e| e.to_string())));
    let rep = delegate::policy_check(regtest, &tcons, &tpp, &[col.clone()], delegate::server_xonly(info))?;
    rec("neg3c_policy_check", format!("delegate spends {}, violations {:?}", rep.delegate_spends, rep.violations));
    ensure!(!rep.violations.is_empty(), "the policy check accepted a colluding-delegate theft");
    Ok(())
}

//! Scenarios `long-offline-*`: "user offline for 2 weeks, asset never lost", scaled to regtest time.
//!
//! Scale: batch expiry 2048 s stands for 28 days (1 day = 73 s); offline window 1024 s = 14 days.
//! * alice: no third party. Long expiry + wallet policy "before going offline, refresh if less than
//!   offline window + margin is left". Offline 14 "days", comes back, VTXO still live, pays Dave.
//! * erin: offline longer than the batch expiry. Registered with watchtowerd A (no keys; presigned
//!   fee ladder paid from Erin's own fee UTXOs). Daemon exits V; Erin comes back, pays Gina on-chain.
//! * frank: registered with watchtowerd B, which is killed before acting (watchtower down) ->
//!   swept -> asset lost; documents what RGB and the wallet health check report.
use crate::forfeit::{BatchOp, colored_batch, first_vtxo, mine_until, spend_exited, summary_rec};
use crate::exit_watchtower::{branch_of, find_spender};
use crate::watchtowerd::{FeeKey, Registration, read_status, registration_dir};
use crate::{ISSUE_AMOUNT, ark_client, ark_client_load, bare_opret, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, rgb_wallet_load, step};
use anyhow::{anyhow, ensure};
use ark_core::server::GetVtxosRequest;
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Address, Amount, Network, OutPoint, Psbt, Transaction, TxOut, Txid};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::{Online, Outpoint, RgbWalletOpsOffline, RgbWalletOpsOnline, Wallet as RgbWallet};
use rgb_lib::{Assignment, ContractId, FileContent};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

pub(crate) const DAY_S: f64 = 2048.0 / 28.0;
const OFFLINE_WINDOW_S: i64 = 1024;
const REFRESH_MARGIN_S: i64 = 200;
const WT_THRESHOLD_S: i64 = 400;
const RATES: [u64; 7] = [1, 2, 4, 8, 16, 32, 64];
const EXIT_SAT: u64 = 20_000;
const PAY_SAT: u64 = 2_000;

fn wtd_dir(name: &str) -> PathBuf {
    crate::paths::watchtowerd().join(name)
}

pub(crate) async fn vtxo_info(v: OutPoint) -> Option<ark_core::server::VirtualTxOutPoint> {
    let mut c = ark_grpc::Client::new("http://localhost:7070".to_string());
    tokio::time::timeout(Duration::from_secs(5), c.connect()).await.ok()?.ok()?;
    tokio::time::timeout(Duration::from_secs(5), c.list_vtxos(GetVtxosRequest::new_for_outpoints(&[v]))).await.ok()?.ok()?.vtxos.into_iter().next()
}

/// Wallet-side health of a colored VTXO (the cheap "flag lost / at risk" part): RGB itself keeps
/// reporting the allocation as settled even after a sweep.
pub(crate) async fn colored_health(v: OutPoint, regtest: &Regtest) -> String {
    if regtest.tx_confirmations(&v.txid).is_some() {
        return "ON-CHAIN (unrolled; safe, spend via exit path)".into();
    }
    match vtxo_info(v).await {
        None => "UNKNOWN (indexer unreachable)".into(),
        Some(i) if i.is_swept => "LOST (batch swept: seal can never be closed; RGB balance is stale)".into(),
        Some(i) if i.is_spent => format!("SPENT/REFRESHED (spent_by {:?})", i.spent_by),
        Some(i) => {
            let left = i.expires_at - now() as i64;
            if left < OFFLINE_WINDOW_S + REFRESH_MARGIN_S {
                format!("AT RISK ({left} s = {:.1} days to expiry)", left as f64 / DAY_S)
            } else {
                format!("HEALTHY ({left} s = {:.1} days to expiry)", left as f64 / DAY_S)
            }
        }
    }
}

fn scenario_path(dir: &Path) -> PathBuf {
    dir.join("scenario.json")
}

pub async fn setup(dir: PathBuf) -> anyhow::Result<()> {
    let state = dir.join("users");
    std::fs::create_dir_all(&state)?;
    let mut rec = summary_rec(dir.join("setup"));
    std::fs::create_dir_all(dir.join("setup"))?;
    let regtest = Arc::new(Regtest::new());
    step("1. users board (erin + hank share a batch -> depth-2 branch)");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (erin, _) = ark_client("erin", regtest.clone(), &state).await?;
    let (hank, _) = ark_client("hank", regtest.clone(), &state).await?;
    let (frank, _) = ark_client("frank", regtest.clone(), &state).await?;
    for n in ["dave", "gina"] {
        bl(|| rgb_wallet(n, &state))?;
    }
    let mut scen = serde_json::Map::new();
    scen.insert("t_setup".into(), now().into());
    scen.insert("state".into(), state.display().to_string().into());
    for (who, client) in [("alice", &alice), ("erin", &erin), ("frank", &frank)] {
        if who == "erin" {
            let (e, h) = tokio::join!(board("erin", &erin, "0.001"), board("hank", &hank, "0.001"));
            e?;
            h?;
        } else {
            board(who, client, "0.001").await?;
        }
        let v = first_vtxo(client).await?;
        let vi = vtxo_info(v).await.ok_or_else(|| anyhow!("no indexer info"))?;
        let left = vi.expires_at - now() as i64;
        rec(&format!("{who}_vtxo"), format!("{v} expires_at {} ({left}s = {:.1} days) batch {:?}", vi.expires_at, left as f64 / DAY_S, vi.commitment_txids));
        ensure!((1500..2200).contains(&left), "expiry not ~2048 s: set vtxoTreeExpiry=2048 via the admin API");
        let (mut w, online) = bl(|| rgb_wallet(who, &state))?;
        let asset_id = onchain_genesis_jump_in(&regtest, &mut w, &online, v, &state, &mut rec).await?;
        rec(&format!("{who}_asset_id"), asset_id.clone());
        let mut v_final = v;
        let mut exp_final = vi.expires_at;
        if who == "alice" {
            // wallet policy before going offline
            let left = vi.expires_at - now() as i64;
            if left < OFFLINE_WINDOW_S + REFRESH_MARGIN_S {
                let (a, _) = client.get_offchain_address().await?;
                let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
                let (c, fc) = colored_batch("alice-prerefresh", client, &w, cid, v, ISSUE_AMOUNT, BatchOp::Refresh(a)).await?;
                mine_until(&regtest, &c).await?;
                v_final = fc.target;
                exp_final = vtxo_info(v_final).await.map(|i| i.expires_at).unwrap_or(0);
                rec("alice_policy", format!("refreshed before going offline ({left}s left)"));
            } else {
                rec("alice_policy", format!("no refresh needed: {left}s ({:.1} days) >= window {OFFLINE_WINDOW_S}s + margin {REFRESH_MARGIN_S}s", left as f64 / DAY_S));
            }
        } else {
            let branch = branch_of(client, v).await?;
            rec(&format!("{who}_exit_branch"), format!("{:?}", branch.iter().map(|t| t.compute_txid()).collect::<Vec<_>>()));
            let sk = SecretKey::new(&mut rand::thread_rng());
            std::fs::write(state.join(format!("{who}-fee-sk.hex")), hex::encode(sk.secret_bytes()))?;
            let fk = FeeKey::new(sk)?;
            let addr = Address::from_script(&fk.spk, Network::Regtest)?;
            for _ in 0..branch.len() {
                regtest::faucet(&addr, "0.0003")?;
            }
            let mut utxos = vec![];
            for _ in 0..20 {
                let r = regtest.rpc("scantxoutset", serde_json::json!(["start", [format!("addr({addr})")]]))?;
                utxos = r["unspents"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|u| -> anyhow::Result<(OutPoint, Amount)> {
                        Ok((OutPoint { txid: Txid::from_str(u["txid"].as_str().unwrap())?, vout: u["vout"].as_u64().unwrap() as u32 }, Amount::from_btc(u["amount"].as_f64().unwrap())?))
                    })
                    .collect::<Result<_, _>>()?;
                if utxos.len() >= branch.len() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            ensure!(utxos.len() >= branch.len(), "fee utxos not found");
            let ladders = branch.iter().zip(utxos.iter()).map(|(t, u)| fk.ladder(t, *u, &RATES)).collect::<Result<Vec<_>, _>>()?;
            let reg = Registration {
                id: format!("{who}-{}", &v.txid.to_string()[..8]),
                vtxo: v.to_string(),
                expires_at: vi.expires_at,
                threshold_s: WT_THRESHOLD_S,
                branch: branch.iter().map(bitcoin::consensus::encode::serialize_hex).collect(),
                ladders,
            };
            let wdir = wtd_dir(if who == "erin" { "A" } else { "B" });
            std::fs::create_dir_all(registration_dir(&wdir))?;
            let path = registration_dir(&wdir).join(format!("{}.json", reg.id));
            let text = serde_json::to_string_pretty(&reg)?;
            std::fs::write(&path, &text)?;
            rec(&format!("{who}_registration"), format!("{} ({} B, {} levels x {} rungs, fee utxos {:?})", path.display(), text.len(), reg.ladders.len(), RATES.len(), utxos));
            // the registration carries no secret
            let seed = std::fs::read_to_string(state.join(format!("{who}-ark-seed.hex")))?;
            let no_secret = !text.contains(&hex::encode(sk.secret_bytes())) && !text.contains(seed.trim());
            rec(&format!("{who}_registration_has_no_secrets"), no_secret.to_string());
            ensure!(no_secret, "secret leaked into registration");
            if who == "erin" {
                // NEG: a malicious watchtower redirects the presigned child's output to itself
                let mut child: Transaction = bitcoin::consensus::encode::deserialize_hex(&reg.ladders[0][1].child)?;
                child.output[0].script_pubkey = bitcoin::ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array_compat([9u8; 20]));
                let r = regtest.rpc("submitpackage", serde_json::json!([[reg.branch[0].clone(), bitcoin::consensus::encode::serialize_hex(&child)], 0.1, 0.0001]));
                let s = format!("{r:?}");
                println!("[neg] redirected fee child: {s}");
                rec("neg_wt_redirect_fee_child", s.chars().take(400).collect());
                ensure!(!s.contains("\"package_msg\": String(\"success\")"), "redirected child accepted!");
            }
        }
        let mut u = serde_json::Map::new();
        u.insert("vtxo".into(), v_final.to_string().into());
        u.insert("expires_at".into(), exp_final.into());
        u.insert("asset_id".into(), asset_id.into());
        u.insert("t_offline".into(), now().into());
        scen.insert(who.into(), u.into());
    }
    std::fs::write(scenario_path(&dir), serde_json::to_string_pretty(&scen)?)?;
    rec("setup_done_at", now().to_string());
    println!("\nSETUP DONE: users go offline now");
    Ok(())
}

trait FromByteArrayCompat {
    fn from_byte_array_compat(b: [u8; 20]) -> Self;
}
impl FromByteArrayCompat for bitcoin::WPubkeyHash {
    fn from_byte_array_compat(b: [u8; 20]) -> Self {
        use bitcoin::hashes::Hash;
        bitcoin::WPubkeyHash::from_byte_array(b)
    }
}

/// On-chain payment of the whole asset from an rgb-lib-owned output `o` (value `value`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pay_from_rgb_output(
    who: &str,
    rgb: &mut RgbWallet,
    online: &Online,
    regtest: &Regtest,
    o: OutPoint,
    value: u64,
    contract_id: ContractId,
    to: bitcoin::ScriptBuf,
    blinding: u64,
    cons: &Path,
) -> anyhow::Result<Txid> {
    let mut input = None;
    for i in 0..10 {
        let _ = bl(|| rgb.list_unspents(Some(online.clone()), false, false));
        match bl(|| rgb.psbt_input_for_utxo(Outpoint { txid: o.txid.to_string(), vout: o.vout })) {
            Ok(inp) => {
                input = Some(inp);
                break;
            }
            Err(e) => {
                println!("[{who}] {o} not in BDK yet ({i}): {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    let input = input.ok_or_else(|| anyhow!("BDK never saw {o}"))?;
    let change = Address::from_str(&bl(|| rgb.get_address())?)?.assume_checked();
    let fee = 600u64;
    let unsigned = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn { previous_output: o, sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME, ..Default::default() }],
        output: vec![
            bare_opret(Amount::ZERO),
            TxOut { value: Amount::from_sat(PAY_SAT), script_pubkey: to },
            TxOut { value: Amount::from_sat(value - PAY_SAT - fee), script_pubkey: change.script_pubkey() },
        ],
    };
    let mut psbt = Psbt::from_unsigned_tx(unsigned)?;
    psbt.inputs[0] = input;
    let t = bl(|| rgb.color_psbt_and_consume_custom(&mut psbt, contract_id, vec![(SealTarget::WitnessVout(1), ISSUE_AMOUNT, blinding)]))?;
    t.save_file(cons).map_err(|e| anyhow!("save: {e}"))?;
    let signed = Psbt::from_str(&bl(|| rgb.sign_psbt(psbt.to_string(), None))?)?;
    let tx = signed.extract_tx().map_err(|e| anyhow!("extract: {e}"))?;
    let txid = regtest.send_raw(&tx)?;
    regtest::mine(1)?;
    Ok(txid)
}

/// Receiver: import the (possibly empty) offline proof, accept, refresh until settled.
pub(crate) async fn receive(who: &str, rgb: &mut RgbWallet, online: &Online, cons: &Path, operators: &str, asset_id: &str, rec: &mut dyn FnMut(&str, String)) -> anyhow::Result<u64> {
    let p = crate::proof::proof_path(cons);
    if p.exists() && crate::proof::psbt_txids(&p)?.len() > 0 {
        let reg = crate::proof::import(rgb, online, &p, operators)?;
        rec(&format!("{who}_proof_registered"), format!("{reg:?}"));
    }
    let mut ok = false;
    for i in 0..10 {
        match bl(|| rgb.provide_out_of_band_consignment(online.clone(), cons.display().to_string(), vec![])) {
            Ok(_) => {
                ok = true;
                break;
            }
            Err(e) => {
                println!("[{who}] provide attempt {i}: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    ensure!(ok, "{who} could not accept");
    let mut settled = 0;
    for _ in 0..6 {
        regtest::mine(1)?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        bl(|| rgb.refresh(online.clone(), None, vec![], false))?;
        let b = bl(|| rgb.get_asset_balance(asset_id.to_string()))?;
        rec(&format!("{who}_balance"), format!("{b:?}"));
        settled = b.settled;
        if settled > 0 {
            break;
        }
    }
    Ok(settled)
}

pub async fn return_user(who: &str, dir: PathBuf) -> anyhow::Result<()> {
    let scen: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(scenario_path(&dir))?)?;
    let state = PathBuf::from(scen["state"].as_str().unwrap());
    let u = &scen[who];
    let v = OutPoint::from_str(u["vtxo"].as_str().unwrap())?;
    let asset_id = u["asset_id"].as_str().unwrap().to_string();
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    let out = dir.join(format!("return-{who}"));
    std::fs::create_dir_all(&out)?;
    let mut rec = summary_rec(out.clone());
    let regtest = Arc::new(Regtest::new());
    let offline_s = now() as i64 - u["t_offline"].as_i64().unwrap();
    rec("offline_for", format!("{offline_s} s = {:.1} scaled days", offline_s as f64 / DAY_S));
    step(&format!("{who} comes back online (new process, wallets reloaded from disk)"));
    let (client, _w) = ark_client_load(who, regtest.clone(), &state).await?;
    let (mut rgb, online) = bl(|| rgb_wallet_load(who, &state))?;
    let health = colored_health(v, &regtest).await;
    rec("health_on_return", health.clone());
    let bal = bl(|| rgb.get_asset_balance(asset_id.clone()));
    rec("rgb_balance_on_return", format!("{bal:?}"));
    let operators = crate::proof::operators(&client.server_info().await?);
    match who {
        "alice" => {
            ensure!(health.starts_with("HEALTHY") || health.starts_with("AT RISK"), "alice's VTXO not live: {health}");
            let exit_addr = Address::from_str(&bl(|| rgb.get_address())?)?.assume_checked();
            let (c, fc) = colored_batch("alice-exit", &client, &rgb, cid, v, ISSUE_AMOUNT, BatchOp::Exit(exit_addr, Amount::from_sat(EXIT_SAT))).await?;
            rec("alice_exit_commitment", c.to_string());
            rec("alice_exit_forfeit", fc.forfeit_tx.compute_txid().to_string());
            mine_until(&regtest, &c).await?;
            let fid = fc.forfeit_tx.compute_txid();
            // Alice registers her own forfeit from an offline proof as well
            let own = out.join("alice_forfeit.rgbc");
            fc.transfer.save_file(&own).map_err(|e| anyhow!("save: {e}"))?;
            crate::proof::build_proof(&client, &regtest, &[fid], &[], &crate::proof::proof_path(&own)).await?;
            crate::proof::import(&rgb, &online, &crate::proof::proof_path(&own), &operators)?;
            let _ = bl(|| rgb.update_witnesses(online.clone(), 0, vec![]));
            let (mut dave, dave_online) = bl(|| rgb_wallet_load("dave", &state))?;
            let recv = bl(|| dave.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
            let to = rgb_lib::utils::script_buf_from_recipient_id(recv.recipient_id.clone())?.ok_or_else(|| anyhow!("not witness"))?;
            let cons = out.join("alice_to_dave.rgbc");
            let ad = pay_from_rgb_output("alice", &mut rgb, &online, &regtest, fc.target, EXIT_SAT, cid, to, 4242, &cons).await?;
            rec("alice_to_dave_txid", ad.to_string());
            let st = crate::proof::build_proof(&client, &regtest, &[fid, ad], &[], &crate::proof::proof_path(&cons)).await?;
            rec("proof_ad", format!("{} B, {} PSBTs", st.bytes, st.n_psbts));
            let settled = receive("dave", &mut dave, &dave_online, &cons, &operators, &asset_id, &mut rec).await?;
            rec("PASS", (settled == ISSUE_AMOUNT).to_string());
            ensure!(settled == ISSUE_AMOUNT, "dave not settled");
        }
        "erin" => {
            let st = read_status(&wtd_dir("A"), &format!("erin-{}", &scen["erin"]["vtxo"].as_str().unwrap()[..8]));
            rec("watchtower_status", format!("{st:?}"));
            ensure!(health.starts_with("ON-CHAIN"), "erin's VTXO not on-chain: {health}");
            let (mut gina, gina_online) = bl(|| rgb_wallet_load("gina", &state))?;
            let recv = bl(|| gina.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
            let gscript = rgb_lib::utils::script_buf_from_recipient_id(recv.recipient_id.clone())?.ok_or_else(|| anyhow!("not witness"))?;
            let gaddr = Address::from_script(&gscript, Network::Regtest)?;
            let cons = out.join("erin_to_gina.rgbc");
            let eg = spend_exited("erin", &client, &rgb, &regtest, v, &gaddr, cid, ISSUE_AMOUNT, 5353, &cons).await?;
            rec("erin_to_gina_txid", eg.to_string());
            regtest::mine(1)?;
            let settled = receive("gina", &mut gina, &gina_online, &cons, &operators, &asset_id, &mut rec).await?;
            rec("PASS", (settled == ISSUE_AMOUNT).to_string());
            ensure!(settled == ISSUE_AMOUNT, "gina not settled");
        }
        "frank" => {
            let branch_leaf_in = {
                let reg: Registration = serde_json::from_str(&std::fs::read_to_string(
                    registration_dir(&wtd_dir("B")).join(format!("frank-{}.json", &v.txid.to_string()[..8])),
                )?)?;
                let leaf: Transaction = bitcoin::consensus::encode::deserialize_hex(reg.branch.last().unwrap())?;
                let r = regtest.rpc("testmempoolaccept", serde_json::json!([[reg.branch.last().unwrap()]]))?;
                rec("frank_leaf_testmempoolaccept", r[0].to_string());
                leaf.input[0].previous_output
            };
            let sweep = find_spender(&regtest, branch_leaf_in, 400)?;
            rec("frank_sweep_tx", format!("{sweep:?} (spends {branch_leaf_in})"));
            let lost = health.starts_with("LOST");
            rec("PASS_control_asset_lost_and_flagged", lost.to_string());
            ensure!(lost, "frank not swept yet: {health}");
        }
        _ => anyhow::bail!("unknown user {who}"),
    }
    println!("\nLONG-OFFLINE RETURN {who} RESULT: PASS");
    Ok(())
}

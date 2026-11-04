//! Scenario `exit-watchtower`: in-process exit watchtower as a safety net against batch expiry.
//!
//! Alice, Erin and Frank board into the same batch (short regtest expiry) and each puts an RGB
//! asset on its VTXO (on-chain genesis + jump-in). The watchtower receives the (already fully
//! signed) exit branches of Alice and Frank, plus nothing else: it has NO user key, only its own
//! P2WPKH fee key. When a watched VTXO is within `THRESHOLD_S` of expiry and the public indexer
//! does not show it refreshed, it broadcasts the branch, CPFP-bumping each tx through its P2A
//! anchor (v3 package via `submitpackage`).
//!  (1) Alice: watchtower exits V before expiry -> Alice spends V on-chain to Gina -> Gina settled.
//!  (2) Erin (control, not watched): the batch is swept; the asset is stuck.
//!  (3) the watchtower cannot redirect the asset (tampered branch / spend of V with its key fail).
//!  (4) Frank refreshes in time: the watchtower does nothing.
use crate::forfeit::{BatchOp, colored_batch, first_vtxo, mine_until, new_state, spend_exited, summary_rec};
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, bail, ensure};
use ark_core::server::GetVtxosRequest;
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::{
    Address, Amount, CompressedPublicKey, Network, OutPoint, PrivateKey, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
    Witness, absolute, transaction,
};
use regtest::Regtest;
use rgb_lib::wallet::{RgbWalletOpsOffline, RgbWalletOpsOnline};
use rgb_lib::{Assignment, ContractId};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

const THRESHOLD_S: i64 = 420;
const WT_FEERATE: u64 = 2; // sat/vB, package feerate target
const BL_GINA: u64 = 808;
const P2A: &str = "51024e73";

pub(crate) struct Watchtower {
    secp: Secp256k1<bitcoin::secp256k1::All>,
    sk: SecretKey,
    pk: CompressedPublicKey,
    spk: ScriptBuf,
    pub(crate) utxo: (OutPoint, Amount),
}

impl Watchtower {
    pub(crate) async fn new(regtest: &Regtest) -> anyhow::Result<Self> {
        let secp = Secp256k1::new();
        let sk = SecretKey::new(&mut rand::thread_rng());
        let pk = CompressedPublicKey::from_private_key(&secp, &PrivateKey::new(sk, Network::Regtest)).map_err(|e| anyhow!("{e}"))?;
        let addr = Address::p2wpkh(&pk, Network::Regtest);
        regtest::faucet(&addr, "0.002")?;
        let spk = addr.script_pubkey();
        for _ in 0..20 {
            let r = regtest.rpc("scantxoutset", serde_json::json!(["start", [format!("addr({addr})")]]))?;
            if let Some(u) = r["unspents"].as_array().and_then(|a| a.first()) {
                let op = OutPoint { txid: Txid::from_str(u["txid"].as_str().unwrap())?, vout: u["vout"].as_u64().unwrap() as u32 };
                let amt = Amount::from_btc(u["amount"].as_f64().unwrap())?;
                println!("[watchtower] fee wallet {addr} funded: {op} {amt}");
                return Ok(Self { secp, sk, pk, spk, utxo: (op, amt) });
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        bail!("watchtower fee wallet not funded")
    }

    /// CPFP child (v3) spending the parent's P2A anchor + the fee UTXO. Pure: the caller commits
    /// the new fee UTXO only when the package is accepted.
    pub(crate) fn bump(&self, parent: &Transaction) -> anyhow::Result<(Transaction, u64)> {
        let anchor = parent
            .output
            .iter()
            .position(|o| o.script_pubkey.to_hex_string() == P2A)
            .ok_or_else(|| anyhow!("no P2A anchor in {}", parent.compute_txid()))?;
        let est_child_vb = 153u64;
        let fee = WT_FEERATE * (parent.vsize() as u64 + est_child_vb);
        let mut tx = Transaction {
            version: transaction::Version(3),
            lock_time: absolute::LockTime::ZERO,
            input: vec![
                TxIn { previous_output: OutPoint { txid: parent.compute_txid(), vout: anchor as u32 }, sequence: Sequence::ENABLE_RBF_NO_LOCKTIME, ..Default::default() },
                TxIn { previous_output: self.utxo.0, sequence: Sequence::ENABLE_RBF_NO_LOCKTIME, ..Default::default() },
            ],
            output: vec![TxOut { value: self.utxo.1 + parent.output[anchor].value - Amount::from_sat(fee), script_pubkey: self.spk.clone() }],
        };
        let h = SighashCache::new(&tx).p2wpkh_signature_hash(1, &self.spk, self.utxo.1, EcdsaSighashType::All)?;
        let sig = self.secp.sign_ecdsa(&Message::from_digest(h.to_byte_array_compat()), &self.sk);
        tx.input[1].witness = Witness::p2wpkh(&bitcoin::ecdsa::Signature::sighash_all(sig), &self.pk.0);
        Ok((tx, fee))
    }
}

trait ToBytes {
    fn to_byte_array_compat(&self) -> [u8; 32];
}
impl ToBytes for bitcoin::SegwitV0Sighash {
    fn to_byte_array_compat(&self) -> [u8; 32] {
        use bitcoin::hashes::Hash;
        self.to_byte_array()
    }
}

#[derive(Debug, PartialEq)]
enum St {
    Watching,
    Exiting,
    Done(String),
}

struct Watch {
    who: &'static str,
    vtxo: OutPoint,
    branch: Vec<Transaction>,
    st: St,
}

pub(crate) fn submit_package(regtest: &Regtest, txs: &[&Transaction]) -> anyhow::Result<serde_json::Value> {
    let hexes: Vec<String> = txs.iter().map(|t| bitcoin::consensus::encode::serialize_hex(*t)).collect();
    regtest.rpc("submitpackage", serde_json::json!([hexes, 0.1, 0.0001]))
}

fn in_mempool(regtest: &Regtest, t: &Txid) -> bool {
    regtest.rpc("getmempoolentry", serde_json::json!([t.to_string()])).is_ok()
}

fn test_accept(regtest: &Regtest, tx: &Transaction) -> String {
    match regtest.rpc("testmempoolaccept", serde_json::json!([[bitcoin::consensus::encode::serialize_hex(tx)]])) {
        Ok(v) => v[0].to_string(),
        Err(e) => format!("rpc error: {e}"),
    }
}

/// One watchtower pass. Uses only public data: arkd's indexer (refresh status, expiry) and bitcoind.
async fn tick(
    wt: &mut Watchtower,
    indexer: &ark_grpc::Client,
    regtest: &Regtest,
    w: &mut Watch,
    rec: &mut dyn FnMut(&str, String),
) -> anyhow::Result<()> {
    let info = indexer.list_vtxos(GetVtxosRequest::new_for_outpoints(&[w.vtxo])).await.ok().and_then(|r| r.vtxos.into_iter().next());
    match w.st {
        St::Watching => {
            let Some(v) = info else {
                println!("[watchtower] {}: indexer unavailable, keep watching", w.who);
                return Ok(());
            };
            let left = v.expires_at - now() as i64;
            if left > THRESHOLD_S {
                return Ok(());
            }
            rec(&format!("wt_{}_decision_at_seconds_left", w.who), left.to_string());
            if v.is_spent && !v.is_unrolled {
                let msg = format!("refreshed in time (spent_by {:?}): no broadcast", v.spent_by);
                println!("[watchtower] {}: {msg}", w.who);
                rec(&format!("wt_{}_action", w.who), msg.clone());
                w.st = St::Done(msg);
            } else if v.is_swept {
                w.st = St::Done("already swept: too late".into());
            } else {
                println!("[watchtower] {}: {left}s to expiry, not refreshed -> exiting", w.who);
                rec(&format!("wt_{}_action", w.who), format!("exit triggered with {left}s left"));
                w.st = St::Exiting;
            }
        }
        St::Exiting => {
            let next = w.branch.iter().find(|t| regtest.tx_confirmations(&t.compute_txid()).is_none());
            let Some(parent) = next else {
                println!("[watchtower] {}: whole branch confirmed", w.who);
                w.st = St::Done("branch on-chain".into());
                return Ok(());
            };
            let pid = parent.compute_txid();
            if in_mempool(regtest, &pid) {
                return Ok(());
            }
            let (child, fee) = wt.bump(parent)?;
            let r = submit_package(regtest, &[parent, &child])?;
            if r["package_msg"].as_str() == Some("success") {
                let cid = child.compute_txid();
                let line = format!(
                    "parent {pid} vsize {} fee 0 | child {cid} vsize {} fee {fee} sat | package {:.2} sat/vB",
                    parent.vsize(), child.vsize(), fee as f64 / (parent.vsize() + child.vsize()) as f64
                );
                println!("[watchtower] {}: broadcast {line}", w.who);
                rec(&format!("wt_{}_hop_{pid}", w.who), line);
                wt.utxo = (OutPoint { txid: cid, vout: 0 }, child.output[0].value);
            } else {
                println!("[watchtower] {}: submitpackage {pid} failed: {r}", w.who);
            }
        }
        St::Done(_) => {}
    }
    Ok(())
}

/// Find the tx spending `op` in the last `depth` blocks.
pub(crate) fn find_spender(regtest: &Regtest, op: OutPoint, depth: u64) -> anyhow::Result<Option<Txid>> {
    let tip = regtest.rpc("getblockcount", serde_json::json!([]))?.as_u64().unwrap();
    for h in (tip.saturating_sub(depth)..=tip).rev() {
        let hash = regtest.rpc("getblockhash", serde_json::json!([h]))?;
        let b = regtest.rpc("getblock", serde_json::json!([hash, 2]))?;
        for tx in b["tx"].as_array().unwrap() {
            for i in tx["vin"].as_array().unwrap() {
                if i["txid"].as_str() == Some(&op.txid.to_string()) && i["vout"].as_u64() == Some(op.vout as u64) {
                    return Ok(Some(Txid::from_str(tx["txid"].as_str().unwrap())?));
                }
            }
        }
    }
    Ok(None)
}

pub(crate) async fn branch_of(client: &ArkClient, v: OutPoint) -> anyhow::Result<Vec<Transaction>> {
    client
        .build_unilateral_exit_trees()
        .await?
        .into_iter()
        .find(|b| b.last().map(|t| t.compute_txid()) == Some(v.txid))
        .ok_or_else(|| anyhow!("no exit branch for {v}"))
}

pub async fn run_watchtower() -> anyhow::Result<()> {
    let state = new_state("exit-watchtower")?;
    let mut rec = summary_rec(state.clone());
    let regtest = Arc::new(Regtest::new());

    step("1. Alice, Erin, Frank board into the same batch (short expiry)");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (erin, _) = ark_client("erin", regtest.clone(), &state).await?;
    let (frank, _) = ark_client("frank", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    rec("arkd_version", info.version.clone());
    rec("unilateral_exit_delay", format!("{:?}", info.unilateral_exit_delay));
    let (a, e, f) = tokio::join!(board("alice", &alice, "0.001"), board("erin", &erin, "0.001"), board("frank", &frank, "0.001"));
    a?;
    e?;
    f?;
    let mut indexer = ark_grpc::Client::new(crate::paths::arkd_url());
    indexer.connect().await?;
    let mut users = vec![];
    for (who, client) in [("alice", &alice), ("erin", &erin), ("frank", &frank)] {
        let v = first_vtxo(client).await?;
        let vi = indexer.list_vtxos(GetVtxosRequest::new_for_outpoints(&[v])).await?.vtxos.into_iter().next().ok_or_else(|| anyhow!("no vtxo"))?;
        rec(&format!("{who}_vtxo"), format!("{v} expires_at {} (in {}s) commitments {:?}", vi.expires_at, vi.expires_at - now() as i64, vi.commitment_txids));
        ensure!(vi.expires_at - (now() as i64) < 2000, "batch expiry is not short: set arkd vtxoTreeExpiry=1024 via the admin settings API");
        let (mut w, online) = bl(|| rgb_wallet(who, &state))?;
        let asset_id = onchain_genesis_jump_in(&regtest, &mut w, &online, v, &state, &mut rec).await?;
        rec(&format!("{who}_asset_id"), asset_id.clone());
        let branch = branch_of(client, v).await?;
        rec(&format!("{who}_exit_branch"), format!("{:?}", branch.iter().map(|t| t.compute_txid()).collect::<Vec<_>>()));
        users.push((who, client, v, w, online, asset_id, branch));
    }
    let [(_, _, va, alice_rgb, _alice_online, asset_a, branch_a), (_, _, ve, mut erin_rgb, erin_online, asset_e, branch_e), (_, _, vf, frank_rgb, _frank_online, asset_f, branch_f)] =
        <[_; 3]>::try_from(users).map_err(|_| anyhow!("users"))?;
    // the registration the user would hand to the watchtower: public, already-signed txs only
    // (protobuf rgbarkade.v1.WatchtowerRegistration, vtxo + branch only; this scenario has no fee ladders)
    for (who, v, branch) in [("alice", &va, &branch_a), ("frank", &vf, &branch_f)] {
        let reg = crate::wire::WatchtowerRegistration {
            id: who.into(),
            vtxo: Some(crate::wire::outpoint(v)),
            branch: branch.iter().map(bitcoin::consensus::encode::serialize).collect(),
            ..Default::default()
        };
        std::fs::write(state.join(format!("watchtower-registration-{who}.pb")), prost::Message::encode_to_vec(&reg))?;
    }
    let mut wt = Watchtower::new(&regtest).await?;
    rec("wt_fee_utxo", format!("{} {}", wt.utxo.0, wt.utxo.1));

    step("2. (test 3a): watchtower tries to redirect a branch tx output to itself");
    let mut tampered = branch_a[0].clone();
    let big = tampered.output.iter().position(|o| o.value.to_sat() > 1000).unwrap();
    tampered.output[big].script_pubkey = wt.spk.clone();
    let (child, _) = wt.bump(&tampered)?;
    let r = submit_package(&regtest, &[&tampered, &child]);
    let s = format!("{r:?}");
    println!("[watchtower] NEG tampered branch package: {s}");
    rec("neg_wt_tampered_branch", s.chars().take(400).collect());
    ensure!(r.map(|v| v["package_msg"].as_str() != Some("success")).unwrap_or(true), "tampered branch accepted!");

    step("3. (test 4): Frank refreshes in time (colored refresh, T_f in the forfeit)");
    let (frank_addr, _) = frank.get_offchain_address().await?;
    let cid_f = ContractId::from_str(&asset_f).map_err(|e| anyhow!("{e}"))?;
    let (cf, fcf) = colored_batch("frank-refresh", &frank, &frank_rgb, cid_f, vf, ISSUE_AMOUNT, BatchOp::Refresh(frank_addr)).await?;
    rec("frank_refresh_commitment", cf.to_string());
    rec("frank_refresh_forfeit", fcf.forfeit_tx.compute_txid().to_string());
    mine_until(&regtest, &cf).await?;

    step("4. watchtower loop until Alice's branch is on-chain and Frank's decision is taken");
    let mut watches = vec![
        Watch { who: "alice", vtxo: va, branch: branch_a.clone(), st: St::Watching },
        Watch { who: "frank", vtxo: vf, branch: branch_f.clone(), st: St::Watching },
    ];
    let deadline = now() + 1500;
    while watches.iter().any(|w| !matches!(w.st, St::Done(_))) {
        ensure!(now() < deadline, "watchtower loop timed out");
        for w in watches.iter_mut() {
            if let Err(e) = tick(&mut wt, &indexer, &regtest, w, &mut rec).await {
                println!("[watchtower] {}: tick error {e:#}", w.who);
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    for w in &watches {
        rec(&format!("wt_{}_final", w.who), format!("{:?}", w.st));
    }
    let frank_onchain: Vec<_> = branch_f.iter().map(|t| (t.compute_txid(), regtest.tx_confirmations(&t.compute_txid()))).collect();
    rec("frank_branch_onchain", format!("{frank_onchain:?}"));
    let frank_untouched = branch_f.iter().all(|t| branch_a.iter().any(|x| x.compute_txid() == t.compute_txid()) || regtest.tx_confirmations(&t.compute_txid()).is_none());
    rec("test4_frank_no_broadcast", frank_untouched.to_string());
    let va_conf = regtest.tx_confirmations(&va.txid);
    rec("alice_V_onchain_confs", format!("{va_conf:?}"));
    ensure!(va_conf.is_some(), "Alice's V not on-chain");

    step("5. (test 3b): watchtower tries to spend Alice's V with its own key");
    let steal = {
        let mut tx = Transaction {
            version: transaction::Version(2),
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn { previous_output: va, sequence: Sequence::ENABLE_RBF_NO_LOCKTIME, ..Default::default() }],
            output: vec![TxOut { value: branch_a.last().unwrap().output[va.vout as usize].value - Amount::from_sat(500), script_pubkey: wt.spk.clone() }],
        };
        let kp = bitcoin::secp256k1::Keypair::from_secret_key(&wt.secp, &wt.sk);
        let sig = wt.secp.sign_schnorr_no_aux_rand(&Message::from_digest([7u8; 32]), &kp);
        tx.input[0].witness = Witness::from_slice(&[sig.as_ref().to_vec()]);
        tx
    };
    let s = test_accept(&regtest, &steal);
    println!("[watchtower] NEG spend V with watchtower key: {s}");
    rec("neg_wt_spend_V", s.clone());
    ensure!(!s.contains("\"allowed\":true"), "watchtower could spend V!");

    step("6. (test 1): Alice spends V on-chain to Gina; Gina accepts");
    let (mut gina_rgb, gina_online) = bl(|| rgb_wallet("gina", &state))?;
    let recv = bl(|| gina_rgb.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
    let gscript = rgb_lib::utils::script_buf_from_recipient_id(recv.recipient_id.clone())?.ok_or_else(|| anyhow!("not witness"))?;
    let gaddr = Address::from_script(&gscript, Network::Regtest)?;
    let cons = state.join("alice_to_gina.rgbc");
    let cid_a = ContractId::from_str(&asset_a).map_err(|e| anyhow!("{e}"))?;
    let ag = spend_exited("alice", &alice, &alice_rgb, &regtest, va, &gaddr, cid_a, ISSUE_AMOUNT, BL_GINA, &cons).await?;
    rec("alice_to_gina_txid", ag.to_string());
    regtest::mine(1)?;
    let mut ok = false;
    for i in 0..10 {
        match bl(|| gina_rgb.provide_out_of_band_consignment(gina_online.clone(), cons.display().to_string(), vec![])) {
            Ok(r) => {
                println!("[gina] accepted {r:?}");
                ok = true;
                break;
            }
            Err(e) => {
                println!("[gina] provide attempt {i}: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    ensure!(ok, "gina could not accept");
    let mut settled = 0;
    for _ in 0..5 {
        regtest::mine(1)?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        bl(|| gina_rgb.refresh(gina_online.clone(), None, vec![], false))?;
        let b = bl(|| gina_rgb.get_asset_balance(asset_a.clone()))?;
        rec("gina_balance", format!("{b:?}"));
        settled = b.settled;
        if settled > 0 {
            break;
        }
    }
    let test1 = settled == ISSUE_AMOUNT;
    rec("test1_PASS", test1.to_string());

    step("7. (test 2, control): Erin was not watched -> batch sweep");
    let mut swept = false;
    let end = now() + 1500;
    while now() < end {
        if let Ok(r) = indexer.list_vtxos(GetVtxosRequest::new_for_outpoints(&[ve])).await {
            if let Some(v) = r.vtxos.first() {
                if v.is_swept {
                    swept = true;
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    rec("erin_vtxo_swept", swept.to_string());
    let root_in = branch_e[0].input[0].previous_output;
    let sweep = find_spender(&regtest, root_in, 300)?;
    rec("erin_batch_output", root_in.to_string());
    rec("erin_batch_output_spent_by", format!("{sweep:?} (alice branch root {})", branch_a[0].compute_txid()));
    // Erin's exit branch: which txs could still go on-chain?
    for t in &branch_e {
        let id = t.compute_txid();
        let s = if regtest.tx_confirmations(&id).is_some() { "already on-chain (shared with Alice's branch)".to_string() } else { test_accept(&regtest, t) };
        rec(&format!("erin_branch_{id}"), s);
    }
    // the sweep spends the outputs of the shared tree that lead to Erin's leaf
    let leaf_in = branch_e.last().unwrap().input[0].previous_output;
    let leaf_spender = find_spender(&regtest, leaf_in, 300)?;
    rec("erin_leaf_input", format!("{leaf_in} spent by {leaf_spender:?}"));
    let bal = bl(|| erin_rgb.get_asset_balance(asset_e.clone()));
    rec("erin_rgb_balance_after_sweep", format!("{bal:?}"));
    let unspents = bl(|| erin_rgb.list_unspents(None, false, false)).map(|u| u.len());
    rec("erin_rgb_unspents", format!("{unspents:?}"));
    // try to move the asset: colored refresh of the swept VTXO (BTC is recoverable, the asset is not)
    let (erin_addr, _) = erin.get_offchain_address().await?;
    let cid_e = ContractId::from_str(&asset_e).map_err(|e| anyhow!("{e}"))?;
    let r = colored_batch("erin-recover", &erin, &erin_rgb, cid_e, ve, ISSUE_AMOUNT, BatchOp::Refresh(erin_addr)).await;
    rec("erin_colored_refresh_after_sweep", format!("{:?}", r.as_ref().map(|x| x.0).map_err(|e| e.to_string())));
    let vt = erin.list_vtxos_filtered(ark_client::GetVtxosRequestFilter::Spendable).await.map(|l| l.spendable_offchain().map(|v| (v.vtxo().outpoint, v.vtxo().amount)).collect::<Vec<_>>());
    rec("erin_btc_vtxos_after_recovery", format!("{vt:?}"));
    let _ = erin_online;
    let test2 = swept && r.is_err();
    rec("test2_control_asset_stuck", test2.to_string());

    let pass = test1 && test2 && frank_untouched;
    rec("PASS", pass.to_string());
    println!("\nEXIT WATCHTOWER RESULT: {}", if pass { "PASS" } else { "FAIL" });
    ensure!(pass, "watchtower tests failed");
    Ok(())
}

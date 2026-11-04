//! Scenario `checkpoint-offline`: a user who goes offline, with ONLY the checkpoint data OP_RETURN
//! in arkd (profile `cponly`). Standard RGB.
//!   1. Alice holds an asset on V (batch 1) and a fresher VTXO G (batch 2)
//!   2. delegated renewal: Alice pre-signs the hop V -> G (her ark tx input and her checkpoint)
//!      and goes offline; a delegate with no Alice key submits it shortly before batch 1 expires.
//!      A delegate that changes the ark tx is refused. Batch 1 is then swept.
//!   3. Alice stays offline past batch 2's expiry: a keyless watchtower, given G's signed exit
//!      branch beforehand, puts G on-chain before the sweep
//!   4. Alice comes back and spends G on-chain to Gina with the commitment in her exit spend.
//!      The renewal checkpoint never reaches the chain, so Gina needs its Ark proof: a plain
//!      RGB check rejects the consignment, with the proof imported Gina settles the asset
use crate::exit_watchtower::{Watchtower, branch_of, submit_package};
use crate::forfeit::spend_exited;
use crate::proof::{build_proof, import, operators, proof_path};
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_client::GetVtxosRequestFilter;
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions};
use bitcoin::{Address, Network, OutPoint, Psbt, ScriptBuf, Transaction, Txid};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::{Online, Outpoint, RgbWalletOpsOffline, RgbWalletOpsOnline, Wallet as RgbWallet};
use rgb_lib::{Assignment, ContractId, FileContent};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/// `vtxoTreeExpiry` of the `cponly` profile, in blocks
const TREE_EXPIRY: u64 = 500;
const BL_HOP: u64 = 9301;
const BL_GINA: u64 = 9302;

/// What Alice hands the delegate: the hop fully built and signed by her, nothing secret.
#[derive(Clone)]
struct Presigned {
    ark_tx: Psbt,
    checkpoints: Vec<Psbt>,
    checkpoint_sigs: Psbt,
    cp_txid: Txid,
    ark_txid: Txid,
}

/// Alice: build the hop `from` -> `to` (commitment in the checkpoint, explicit seal on `to`) and
/// sign everything that needs her key. The consignment is saved to `consignment`.
async fn presign_hop(ark: &ArkClient, rgb: &RgbWallet, from: OutPoint, cid: ContractId, to: OutPoint, consignment: &std::path::Path) -> anyhow::Result<Presigned> {
    let info = ark.server_info().await?;
    let input = ark.resolve_selected_send_inputs(&[from]).await?.remove(0);
    let (me, _) = ark.get_offchain_address().await?;
    let receivers = vec![SendReceiver::bitcoin(me.clone(), input.amount())];
    let dry = build_offchain_transactions(&receivers, &me, &[input.clone().with_checkpoint_op_return(Some([0u8; 32]))], &info)?;
    let mut cp = dry.checkpoint_txs[0].clone();
    cp.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    let seal = SealTarget::Explicit(Outpoint { txid: to.txid.to_string(), vout: to.vout });
    let transfer = bl(|| rgb.color_psbt_and_consume_custom(&mut cp, cid, vec![(seal, ISSUE_AMOUNT, BL_HOP)]))?;
    transfer.save_file(consignment).map_err(|e| anyhow!("save consignment: {e}"))?;
    let commitment = cp.unsigned_tx.output[0].script_pubkey.clone();
    ensure!(commitment.is_op_return() && commitment.len() == 34, "bad commitment {commitment}");
    let mut payload = [0u8; 32];
    payload.copy_from_slice(&commitment.as_bytes()[2..]);
    let OffchainTransactions { mut ark_tx, checkpoint_txs } =
        build_offchain_transactions(&receivers, &me, &[input.with_checkpoint_op_return(Some(payload))], &info)?;
    let cp_txid = checkpoint_txs[0].unsigned_tx.compute_txid();
    ensure!(cp_txid == cp.unsigned_tx.compute_txid(), "checkpoint changed between the two builds");
    ark.sign_ark_tx_inputs(&mut ark_tx, &[0]).map_err(|e| anyhow!("sign ark tx: {e:?}"))?;
    let mut checkpoint_sigs = checkpoint_txs[0].clone();
    if checkpoint_sigs.inputs[0].witness_script.is_none() {
        checkpoint_sigs.inputs[0].witness_script = ark_tx.inputs[0].witness_script.clone();
    }
    ark.sign_checkpoint_tx(&mut checkpoint_sigs).map_err(|e| anyhow!("sign checkpoint: {e:?}"))?;
    ensure!(!checkpoint_sigs.inputs[0].tap_script_sigs.is_empty(), "could not pre-sign the checkpoint");
    let ark_txid = ark_tx.unsigned_tx.compute_txid();
    Ok(Presigned { ark_tx, checkpoints: checkpoint_txs, checkpoint_sigs, cp_txid, ark_txid })
}

/// The delegate: submit the pre-signed hop and finalize it with Alice's checkpoint signature.
/// Its own client only talks to arkd; none of its keys sign anything.
async fn delegate_submit(delegate: &ArkClient, p: &Presigned) -> anyhow::Result<()> {
    let res = delegate
        .network_client()
        .submit_offchain_transaction_request(p.ark_tx.clone(), p.checkpoints.clone())
        .await
        .map_err(|e| anyhow!("submit: {e:?}"))?;
    ensure!(res.signed_ark_tx.unsigned_tx.compute_txid() == p.ark_txid, "arkd rebuilt a different ark tx");
    let mut cps = res.signed_checkpoint_txs;
    for cp in cps.iter_mut() {
        if cp.inputs[0].witness_script.is_none() {
            cp.inputs[0].witness_script = res.signed_ark_tx.inputs[0].witness_script.clone();
        }
        cp.inputs[0].tap_script_sigs.extend(p.checkpoint_sigs.inputs[0].tap_script_sigs.clone());
    }
    delegate.finalize_offchain_tx(p.ark_txid, cps).await.map_err(|e| anyhow!("finalize: {e:?}"))?;
    Ok(())
}

fn height(regtest: &Regtest) -> anyhow::Result<u64> {
    regtest.rpc("getblockcount", serde_json::json!([]))?.as_u64().ok_or_else(|| anyhow!("getblockcount"))
}

fn mine_to(regtest: &Regtest, h: u64) -> anyhow::Result<()> {
    let now_h = height(regtest)?;
    if h > now_h {
        regtest::mine((h - now_h) as u32)?;
    }
    Ok(())
}

async fn first(ark: &ArkClient, who: &str) -> anyhow::Result<OutPoint> {
    ark.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().next().map(|v| v.vtxo().outpoint).ok_or_else(|| anyhow!("{who} has no VTXO"))
}

/// Watchtower: broadcast every unconfirmed tx of `branch`, each CPFP-bumped through its anchor.
pub(crate) async fn unroll(wt: &mut Watchtower, regtest: &Regtest, branch: &[Transaction], rec: &mut dyn FnMut(&str, String)) -> anyhow::Result<()> {
    for parent in branch {
        let pid = parent.compute_txid();
        if regtest.tx_confirmations(&pid).is_some() {
            continue;
        }
        let (child, fee) = wt.bump(parent)?;
        let r = submit_package(regtest, &[parent, &child])?;
        ensure!(r["package_msg"].as_str() == Some("success"), "package for {pid} refused: {r}");
        wt.utxo = (OutPoint { txid: child.compute_txid(), vout: 0 }, child.output[0].value);
        rec(&format!("wt_broadcast_{pid}"), format!("child {} fee {fee} sat", child.compute_txid()));
        regtest::mine(1)?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        ensure!(regtest.tx_confirmations(&pid).is_some(), "{pid} not confirmed");
    }
    Ok(())
}

pub async fn run() -> anyhow::Result<()> {
    let run_id = format!("run-{}-checkpoint-offline", now());
    let state = crate::paths::state().join(&run_id);
    std::fs::create_dir_all(&state)?;
    println!("run id {run_id}, state dir {}", state.display());
    let mut summary = serde_json::Map::new();
    let mut rec = |k: &str, v: String| {
        println!("  >> {k} = {v}");
        summary.insert(k.to_string(), serde_json::Value::String(v));
        std::fs::write(state.join("summary.json"), serde_json::to_string_pretty(&summary).unwrap()).unwrap();
    };
    let regtest = Arc::new(Regtest::new());

    step("1. Alice: asset on V (batch 1); 240 blocks later she boards G (batch 2)");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    rec("arkd_version", info.version.clone());
    board("alice", &alice, "0.001").await?;
    let h1 = height(&regtest)?;
    let v = first(&alice, "alice").await?;
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, v, &state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    regtest::mine(240)?;
    let (alice_g, _) = ark_client("alice-g", regtest.clone(), &state).await?;
    board("alice-g", &alice_g, "0.0005").await?;
    let h2 = height(&regtest)?;
    let g = first(&alice_g, "alice-g").await?;
    rec("vtxos", format!("V {v} (boarded at height {h1}), G {g} (boarded at height {h2}), tree expiry {TREE_EXPIRY} blocks"));

    step("2. Alice pre-signs the renewal V -> G and G's exit branch, hands them over, goes offline");
    let consignment = state.join("v-to-g.rgbc");
    let p = presign_hop(&alice, &alice_rgb, v, cid, g, &consignment).await?;
    rec("presigned_hop", format!("checkpoint {} ark tx {}", p.cp_txid, p.ark_txid));
    let branch_g = branch_of(&alice_g, g).await?;
    rec("g_exit_branch", format!("{:?}", branch_g.iter().map(|t| t.compute_txid()).collect::<Vec<_>>()));
    drop(alice);
    drop(alice_g);

    step("3. the delegate waits until batch 1 is about to expire");
    let (delegate, _) = ark_client("delegate", regtest.clone(), &state).await?;
    mine_to(&regtest, h1 + TREE_EXPIRY - 30)?;
    let left = crate::long_offline::vtxo_info(v).await;
    rec("v_before_delegate", format!("height {} spent {:?} swept {:?}", height(&regtest)?, left.as_ref().map(|x| x.is_spent), left.as_ref().map(|x| x.is_swept)));

    step("3a. negative: the delegate sends the bitcoin to itself instead");
    let (thief, _) = delegate.get_offchain_address().await?;
    let mut tampered = p.clone();
    let out = tampered.ark_tx.unsigned_tx.output.iter().position(|o| o.value.to_sat() > 1000).ok_or_else(|| anyhow!("no value output"))?;
    tampered.ark_tx.unsigned_tx.output[out].script_pubkey = thief.to_p2tr_script_pubkey();
    tampered.ark_txid = tampered.ark_tx.unsigned_tx.compute_txid();
    let r = delegate_submit(&delegate, &tampered).await;
    rec("neg_delegate_redirect", format!("{:?}", r.as_ref().err().map(|e| e.to_string().chars().take(300).collect::<String>())));
    ensure!(r.is_err(), "arkd accepted a tampered pre-signed hop");

    step("3b. the delegate submits Alice's pre-signed renewal");
    delegate_submit(&delegate, &p).await?;
    rec("delegated_renewal", format!("ark tx {} submitted {} blocks after Alice signed", p.ark_txid, height(&regtest)? - h2));

    step("3c. batch 1 expires and is swept");
    mine_to(&regtest, h1 + TREE_EXPIRY + 20)?;
    let left = OutPoint { txid: p.ark_txid, vout: 0 };
    let mut swept = false;
    for _ in 0..40 {
        if crate::long_offline::vtxo_info(left).await.is_some_and(|x| x.is_swept) {
            swept = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    rec("batch1_swept", format!("{left} swept: {swept}"));
    ensure!(swept, "batch 1 was not swept");

    step("4. Alice still offline; shortly before batch 2 expires the watchtower puts G on-chain");
    mine_to(&regtest, h2 + TREE_EXPIRY - 40)?;
    let mut wt = Watchtower::new(&regtest).await?;
    unroll(&mut wt, &regtest, &branch_g, &mut rec).await?;
    rec("g_onchain_confs", format!("{:?}", regtest.tx_confirmations(&g.txid)));
    mine_to(&regtest, h2 + TREE_EXPIRY + 20)?;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let gi = crate::long_offline::vtxo_info(g).await;
    rec("g_after_batch2_expiry", format!("unrolled {:?} swept {:?}", gi.as_ref().map(|x| x.is_unrolled), gi.as_ref().map(|x| x.is_swept)));

    step("5. Alice is back (wallet reloaded from her seed): she spends G on-chain to Gina, the commitment in her exit spend");
    let (alice_g, _) = crate::common::ark_client_load("alice-g", regtest.clone(), &state).await?;
    let (mut gina_rgb, gina_online) = bl(|| rgb_wallet("gina", &state))?;
    let recv = bl(|| gina_rgb.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
    let gscript = rgb_lib::utils::script_buf_from_recipient_id(recv.recipient_id.clone())?.ok_or_else(|| anyhow!("not witness"))?;
    let gaddr = Address::from_script(&gscript, Network::Regtest)?;
    let to_gina = state.join("g-to-gina.rgbc");
    let spend = spend_exited("alice", &alice_g, &alice_rgb, &regtest, g, &gaddr, cid, ISSUE_AMOUNT, BL_GINA, &to_gina).await?;
    rec("alice_to_gina_txid", spend.to_string());
    regtest::mine(1)?;

    step("6. Gina: plain RGB check, then with the Ark proof of the renewal checkpoint");
    let plain = bl(|| gina_rgb.validate_consignment_ark(gina_online.clone(), to_gina.clone(), false));
    rec("neg_plain_rgb_rejects", format!("{:?}", plain.as_ref().err().map(|e| e.to_string())));
    ensure!(plain.is_err(), "a plain RGB check accepted a history with an off-chain checkpoint");
    let pp = proof_path(&to_gina);
    build_proof(&alice_g, &regtest, &[p.cp_txid], &[], &pp).await?;
    let reg = import(&gina_rgb, &gina_online, &pp, &operators(&info))?;
    rec("gina_proof_registered", format!("{reg:?}"));
    settle(&mut gina_rgb, &gina_online, &to_gina, &asset_id, &mut rec).await?;
    println!("CHECKPOINT OFFLINE RESULT: PASS");
    Ok(())
}

pub(crate) async fn settle(gina: &mut RgbWallet, online: &Online, cons: &std::path::Path, asset_id: &str, rec: &mut dyn FnMut(&str, String)) -> anyhow::Result<()> {
    let mut accepted = None;
    for i in 0..10 {
        match bl(|| gina.provide_out_of_band_consignment(online.clone(), cons.display().to_string(), vec![])) {
            Ok(r) => {
                accepted = Some(format!("{r:?}"));
                break;
            }
            Err(e) => {
                println!("[gina] provide attempt {i}: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    rec("gina_accepted", format!("{accepted:?}"));
    ensure!(accepted.is_some(), "Gina could not accept");
    let mut settled = 0;
    for _ in 0..6 {
        regtest::mine(1)?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        bl(|| gina.refresh(online.clone(), None, vec![], false))?;
        let b = bl(|| gina.get_asset_balance(asset_id.to_string()))?;
        settled = b.settled;
        if settled > 0 {
            break;
        }
    }
    rec("gina_settled", settled.to_string());
    ensure!(settled == ISSUE_AMOUNT, "Gina settled {settled}, expected {ISSUE_AMOUNT}");
    Ok(())
}

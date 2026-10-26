//! Scenario `offchain-transfer`: multi-hop off-chain RGB transfers on Arkade (requires the patched `arkd:rgb`).
//!
//! jump-in (on-chain genesis -> explicit seal on Alice's VTXO V)
//!   -> hop A->B (T1 in checkpoint: V -> CP_A:1; T2 in ark tx: CP_A:1 -> Bob)
//!   -> hop B->C (T1: Bob VTXO -> CP_B:1; T2: CP_B:1 -> Carol 700 + Bob change 300)
//!   -> Carol unilateral exit -> Carol sends on-chain to Dave. PASS = Dave settled balance.
use crate::{
    ArkClient, ISSUE_AMOUNT, ark_client, bare_opret, bl, board, color_and_commit, now,
    onchain_genesis_jump_in, regtest, rgb_wallet, step,
};
use anyhow::{anyhow, bail, ensure};
use ark_client::GetVtxosRequestFilter;
use ark_core::ArkAddress;
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions};
use bitcoin::key::XOnlyPublicKey;
use bitcoin::{Address, Amount, Network, OutPoint, Psbt, ScriptBuf, Txid, psbt};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::{RgbWalletOpsOffline, RgbWalletOpsOnline, Wallet as RgbWallet};
use ark_client::wallet::OnchainWallet;
use rgb_lib::{Assignment, ContractId, FileContent, RgbTransfer};
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

const ALICE_TO_BOB_SAT: u64 = 20_000;
const BOB_TO_CAROL_SAT: u64 = 5_000;
const CAROL_ASSET: u64 = 700;
const BOB_ASSET_CHANGE: u64 = ISSUE_AMOUNT - CAROL_ASSET;
const CAROL_ONCHAIN_BTC: &str = "0.01";
const DAVE_SAT: u64 = 2_000;
const CAROL_EXIT_SPEND_FEE_SAT: u64 = 600;

const BL_CP_A: u64 = 101;
const BL_BOB: u64 = 202;
const BL_CP_B: u64 = 303;
const BL_CAROL: u64 = 404;
const BL_BOB_CHANGE: u64 = 505;
const BL_DAVE: u64 = 606;

pub(crate) struct HopResult {
    pub(crate) checkpoint_txid: Txid,
    pub(crate) ark_txid: Txid,
    pub(crate) consignment: PathBuf,
}

pub(crate) fn outs(p: &Psbt) -> Vec<(u64, String)> {
    p.unsigned_tx.output.iter().map(|o| (o.value.to_sat(), o.script_pubkey.to_hex_string())).collect()
}

/// One RGB-aware off-chain hop. `asset_outs` are (ark-tx vout *after* the opret insertion at
/// index 0, amount, blinding): receivers occupy vouts 1..=n, BTC change (if any) n+1.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn rgb_hop(
    label: &str,
    sender: &ArkClient,
    sender_rgb: &RgbWallet,
    vtxo: OutPoint,
    receivers: Vec<SendReceiver>,
    change_addr: &ArkAddress,
    change_pk: XOnlyPublicKey,
    contract_id: ContractId,
    asset_outs: Vec<(u32, u64, u64)>,
    cp_blinding: u64,
    consignment_path: PathBuf,
) -> anyhow::Result<HopResult> {
    let info = sender.server_info().await?;
    let inputs = sender.resolve_selected_send_inputs(&[vtxo]).await?;
    ensure!(inputs.len() == 1, "expected exactly one input");

    // (1) T1 in the checkpoint tx: VTXO seal -> witness vout 1 (CP:1, the checkpoint output).
    // Build with a placeholder payload to get the exact output layout, then color a clone whose
    // OP_RETURN is bare (rgb-lib's opret host) to obtain the real 32-byte commitment.
    let placeholder: Vec<_> = inputs.iter().cloned().map(|i| i.with_checkpoint_op_return(Some([0u8; 32]))).collect();
    let dry = build_offchain_transactions(&receivers, change_addr, &placeholder, &info)?;
    let mut cp_colored = dry.checkpoint_txs[0].clone();
    ensure!(cp_colored.unsigned_tx.output.len() == 3, "checkpoint layout {:?}", outs(&cp_colored));
    // layout with the payload: [OP_RETURN (opret1st host), checkpoint (CP:1), anchor]
    cp_colored.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    let total: u64 = asset_outs.iter().map(|(_, a, _)| a).sum();
    let _t1 = bl(|| {
        sender_rgb.color_psbt_and_consume_custom(&mut cp_colored, contract_id, vec![(SealTarget::WitnessVout(1), total, cp_blinding)])
    })?;
    let commitment = cp_colored.unsigned_tx.output[0].script_pubkey.clone();
    ensure!(commitment.is_op_return() && commitment.len() == 34, "bad T1 commitment {commitment}");
    let mut payload = [0u8; 32];
    payload.copy_from_slice(&commitment.as_bytes()[2..]);
    let cp_txid_colored = cp_colored.unsigned_tx.compute_txid();
    println!("[{label}] T1 committed in checkpoint {cp_txid_colored} (opret {})", commitment.to_hex_string());

    // (2) real build with the commitment as the checkpoint OP_RETURN payload
    let inputs: Vec<_> = inputs.into_iter().map(|i| i.with_checkpoint_op_return(Some(payload))).collect();
    let OffchainTransactions { mut ark_tx, checkpoint_txs } =
        build_offchain_transactions(&receivers, change_addr, &inputs, &info)?;
    let cp_txid = checkpoint_txs[0].unsigned_tx.compute_txid();
    ensure!(cp_txid == cp_txid_colored, "checkpoint txid {cp_txid} != colored {cp_txid_colored}");
    println!("[{label}] checkpoint outputs {:?}", outs(&checkpoint_txs[0]));

    // (3) T2 in the ark tx: CP:1 -> receiver(s)/change. 0-value OP_RETURN at index 0 (arkd: 0-sat data output).
    ark_tx.unsigned_tx.output.insert(0, bare_opret(Amount::ZERO));
    ark_tx.outputs.insert(0, psbt::Output::default());
    let mut ark_colored = ark_tx.clone();
    let beneficiaries = asset_outs.iter().map(|(v, a, b)| (SealTarget::WitnessVout(*v), *a, *b)).collect();
    let t2: RgbTransfer = bl(|| sender_rgb.color_psbt_and_consume_custom(&mut ark_colored, contract_id, beneficiaries))?;
    ark_tx.unsigned_tx.output[0].script_pubkey = ark_colored.unsigned_tx.output[0].script_pubkey.clone();
    let ark_txid = ark_tx.unsigned_tx.compute_txid();
    ensure!(ark_txid == ark_colored.unsigned_tx.compute_txid(), "ark txid mismatch after commit");
    println!("[{label}] ark tx outputs {:?}", outs(&ark_tx));
    t2.save_file(&consignment_path).map_err(|e| anyhow!("save consignment: {e}"))?;

    // (4) submit + finalize; the server must reproduce both txids
    let pending = sender
        .submit_built_offchain_send(ark_tx, checkpoint_txs, change_pk)
        .await
        .map_err(|e| anyhow!("[{label}] submit: {e:?}"))?;
    let server_cp: Vec<Txid> = pending.signed_checkpoint_txs.iter().map(|p| p.unsigned_tx.compute_txid()).collect();
    ensure!(pending.ark_txid == ark_txid, "server ark txid {} != local {ark_txid}", pending.ark_txid);
    ensure!(server_cp == vec![cp_txid], "server checkpoints {server_cp:?} != {cp_txid}");
    sender.sign_and_finalize_pending_tx(pending).await.map_err(|e| anyhow!("[{label}] finalize: {e:?}"))?;
    println!("[{label}] finalized: checkpoint {cp_txid}, ark tx {ark_txid}");
    Ok(HopResult { checkpoint_txid: cp_txid, ark_txid, consignment: consignment_path })
}

/// Cross-check with arkd (GetVirtualTxs) that every offchain witness is a known virtual tx.
pub(crate) async fn arkd_virtual_txs_known(client: &ArkClient, txids: &[Txid]) -> anyhow::Result<()> {
    let resp = client
        .network_client()
        .get_virtual_txs(txids.iter().map(|t| t.to_string()).collect(), None, None)
        .await
        .map_err(|e| anyhow!("get_virtual_txs: {e:?}"))?;
    let got: HashSet<Txid> = resp.txs.iter().map(|p| p.unsigned_tx.compute_txid()).collect();
    for t in txids {
        ensure!(got.contains(t), "arkd does not know virtual tx {t}");
    }
    println!("arkd GetVirtualTxs confirms {} offchain witnesses", txids.len());
    Ok(())
}

pub(crate) async fn wait_vtxo(client: &ArkClient, outpoint: OutPoint, who: &str) -> anyhow::Result<()> {
    for _ in 0..20 {
        let l = client.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?;
        if l.spendable_offchain().any(|v| v.vtxo().outpoint == outpoint) {
            println!("[{who}] sees VTXO {outpoint}");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("[{who}] VTXO {outpoint} not found")
}

pub async fn run() -> anyhow::Result<()> {
    let run_id = format!("run-{}-offchain-transfer", now());
    let state = crate::paths::state().join(&run_id);
    std::fs::create_dir_all(&state)?;
    println!("run id {run_id}, state dir {}", state.display());
    let mut summary = serde_json::Map::new();
    let rec = |k: &str, v: String, summary: &mut serde_json::Map<String, serde_json::Value>| {
        println!("  >> {k} = {v}");
        summary.insert(k.to_string(), serde_json::Value::String(v));
        std::fs::write(state.join("summary.json"), serde_json::to_string_pretty(summary).unwrap()).unwrap();
    };
    let regtest = Arc::new(Regtest::new());

    step("1. clients, Alice boards");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    let (carol, carol_w) = ark_client("carol", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    println!("arkd: dust {} vtxo_min_amount {:?} unilateral_exit_delay {:?}", info.dust, info.vtxo_min_amount, info.unilateral_exit_delay);
    board("alice", &alice, "0.001").await?;
    let v = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?
        .spendable_offchain().next().ok_or_else(|| anyhow!("alice has no VTXO"))?.vtxo().clone();
    rec("alice_vtxo_V", v.outpoint.to_string(), &mut summary);
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;
    let (mut carol_rgb, carol_online) = bl(|| rgb_wallet("carol", &state))?;
    let (mut dave_rgb, dave_online) = bl(|| rgb_wallet("dave", &state))?;

    step("2. on-chain genesis + jump-in to Alice's VTXO V");
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, v.outpoint, &state, &mut |k, val| rec(k, val, &mut summary)).await?;
    rec("asset_id", asset_id.clone(), &mut summary);
    let contract_id = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;

    step("3. hop Alice -> Bob (T1 in checkpoint, T2 in ark tx)");
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (alice_change, alice_change_vtxo) = alice.get_offchain_address().await?;
    let ab = rgb_hop(
        "A->B", &alice, &alice_rgb, v.outpoint,
        vec![SendReceiver::bitcoin(bob_addr, Amount::from_sat(ALICE_TO_BOB_SAT))],
        &alice_change, alice_change_vtxo.owner_pk(), contract_id,
        vec![(1, ISSUE_AMOUNT, BL_BOB)], BL_CP_A, state.join("alice_to_bob.rgbc"),
    ).await?;
    rec("hop_ab_checkpoint_txid", ab.checkpoint_txid.to_string(), &mut summary);
    rec("hop_ab_ark_txid", ab.ark_txid.to_string(), &mut summary);
    let bob_vtxo = OutPoint { txid: ab.ark_txid, vout: 1 };
    wait_vtxo(&bob, bob_vtxo, "bob").await?;

    let ab_witnesses = vec![ab.checkpoint_txid, ab.ark_txid];
    let operators = crate::proof::operators(&info);
    let offline = crate::proof::offline_mode();
    rec("offline_proof_mode", offline.to_string(), &mut summary);
    let p_ab = crate::proof::proof_path(&ab.consignment);
    let (_, bob_assign) = if offline {
        let st = crate::proof::build_proof(&alice, &regtest, &ab_witnesses, &[], &p_ab).await?;
        rec("proof_ab", format!("{} B, {} PSBTs, {} witnesses (consignment {} B)", st.bytes, st.n_psbts, st.n_witnesses, crate::proof::file_len(&ab.consignment)), &mut summary);
        let _down = crate::proof::arkd_down(&mut |k: &str, v: String| rec(k, v, &mut summary), "bob")?;
        let neg = crate::proof::negatives("bob", &bob_rgb, &bob_online, &p_ab, &operators, &mut |k: &str, v: String| rec(k, v, &mut summary))?;
        ensure!(neg, "a mutated proof was accepted");
        let reg = crate::proof::import(&bob_rgb, &bob_online, &p_ab, &operators)?;
        rec("bob_proof_registered", format!("{reg:?}"), &mut summary);
        let r = bl(|| bob_rgb.accept_transfer_consignment_offchain(
            bob_online.clone(), ab.consignment.clone(), vec![], ab.ark_txid.to_string(), 1, BL_BOB,
        ))?;
        r
    } else {
        arkd_virtual_txs_known(&bob, &ab_witnesses).await?;
        bl(|| bob_rgb.accept_transfer_consignment_offchain(
            bob_online.clone(), ab.consignment.clone(), ab_witnesses.iter().map(|t| t.to_string()).collect(),
            ab.ark_txid.to_string(), 1, BL_BOB,
        ))?
    };
    rec("bob_accept_assignments", format!("{bob_assign:?}"), &mut summary);

    step("4. hop Bob -> Carol (700 to Carol, 300 asset change to Bob)");
    let (carol_addr, _) = carol.get_offchain_address().await?;
    let (bob_change, bob_change_vtxo) = bob.get_offchain_address().await?;
    let bc = rgb_hop(
        "B->C", &bob, &bob_rgb, bob_vtxo,
        vec![SendReceiver::bitcoin(carol_addr, Amount::from_sat(BOB_TO_CAROL_SAT))],
        &bob_change, bob_change_vtxo.owner_pk(), contract_id,
        vec![(1, CAROL_ASSET, BL_CAROL), (2, BOB_ASSET_CHANGE, BL_BOB_CHANGE)], BL_CP_B, state.join("bob_to_carol.rgbc"),
    ).await?;
    rec("hop_bc_checkpoint_txid", bc.checkpoint_txid.to_string(), &mut summary);
    rec("hop_bc_ark_txid", bc.ark_txid.to_string(), &mut summary);
    let carol_vtxo = OutPoint { txid: bc.ark_txid, vout: 1 };
    wait_vtxo(&carol, carol_vtxo, "carol").await?;
    let all_witnesses = vec![ab.checkpoint_txid, ab.ark_txid, bc.checkpoint_txid, bc.ark_txid];
    let (_, carol_assign) = if offline {
        let p_bc = crate::proof::proof_path(&bc.consignment);
        let st = crate::proof::build_proof(&bob, &regtest, &all_witnesses, &[p_ab.clone()], &p_bc).await?;
        rec("proof_bc", format!("{} B, {} PSBTs, {} witnesses (consignment {} B)", st.bytes, st.n_psbts, st.n_witnesses, crate::proof::file_len(&bc.consignment)), &mut summary);
        let _down = crate::proof::arkd_down(&mut |k: &str, v: String| rec(k, v, &mut summary), "carol")?;
        let reg = crate::proof::import(&carol_rgb, &carol_online, &p_bc, &operators)?;
        rec("carol_proof_registered", format!("{reg:?}"), &mut summary);
        let r = bl(|| carol_rgb.accept_transfer_consignment_offchain(
            carol_online.clone(), bc.consignment.clone(), vec![], bc.ark_txid.to_string(), 1, BL_CAROL,
        ))?;
        r
    } else {
        arkd_virtual_txs_known(&carol, &all_witnesses).await?;
        bl(|| carol_rgb.accept_transfer_consignment_offchain(
            carol_online.clone(), bc.consignment.clone(), all_witnesses.iter().map(|t| t.to_string()).collect(),
            bc.ark_txid.to_string(), 1, BL_CAROL,
        ))?
    };
    rec("carol_accept_assignments", format!("{carol_assign:?}"), &mut summary);
    let bob_assets = bl(|| bob_rgb.list_assets(vec![]))?;
    println!("[bob] list_assets after hop B->C: {:?}", bob_assets.nia);

    step("5. Carol unilateral exit of the RGB VTXO");
    let carol_onchain = carol.get_onchain_address()?;
    regtest::faucet(&carol_onchain, CAROL_ONCHAIN_BTC)?;
    let mut funded = false;
    for _ in 0..40 {
        carol_w.sync().await?;
        if carol_w.balance()?.confirmed > Amount::ZERO {
            funded = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    ensure!(funded, "carol on-chain wallet never saw the faucet funds");
    let trees = carol.build_unilateral_exit_trees().await?;
    let branch = trees
        .into_iter()
        .find(|b| b.last().map(|t| t.compute_txid()) == Some(bc.ark_txid))
        .ok_or_else(|| anyhow!("no exit branch ending in ark tx B->C"))?;
    let branch_ids: Vec<String> = branch.iter().map(|t| t.compute_txid().to_string()).collect();
    rec("exit_branch", format!("{branch_ids:?}"), &mut summary);
    for _ in 0..(branch.len() + 8) {
        carol_w.sync().await?;
        match carol.broadcast_next_unilateral_exit_node(&branch).await {
            Ok(Some(txid)) => {
                println!("[carol] broadcast exit node {txid}");
                regtest::mine(1)?;
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Ok(None) => break,
            Err(e) => bail!("exit broadcast failed: {e:?}"),
        }
    }
    for t in &all_witnesses {
        let c = regtest.tx_confirmations(t);
        rec(&format!("onchain_confs_{t}"), format!("{c:?}"), &mut summary);
        ensure!(c.is_some(), "offchain witness {t} not on chain after exit");
    }
    let upd = bl(|| carol_rgb.update_witnesses(carol_online.clone(), 0, vec![]))?;
    println!("[carol] update_witnesses: {upd:?}");

    step("6. after the exit delay, Carol sends on-chain to Dave");
    let dave_recv = bl(|| dave_rgb.witness_receive(None, Assignment::Fungible(CAROL_ASSET), now() + 86_400, vec![], 1))?;
    let dave_script = rgb_lib::utils::script_buf_from_recipient_id(dave_recv.recipient_id.clone())?
        .ok_or_else(|| anyhow!("not a witness recipient"))?;
    let dave_addr = Address::from_script(&dave_script, Network::Regtest)?;
    rec("dave_recipient_id", dave_recv.recipient_id.clone(), &mut summary);
    let mut carol_transfers: Option<Vec<RgbTransfer>> = None;
    let mut spend = None;
    for attempt in 0..120 {
        let slot = &mut carol_transfers;
        let rgb_ref = &carol_rgb;
        let hook = move |p: &mut Psbt| -> Result<(), ark_core::Error> {
            let (t, _) = color_and_commit(rgb_ref, p, contract_id, 1, CAROL_ASSET, BL_DAVE)
                .map_err(|e| ark_core::Error::ad_hoc(format!("rgb coloring failed: {e:#}")))?;
            *slot = Some(t);
            Ok(())
        };
        match carol
            .create_send_on_chain_transaction_with_hook(dave_addr.clone(), Amount::from_sat(DAVE_SAT), vec![bare_opret(Amount::ZERO)], Amount::from_sat(CAROL_EXIT_SPEND_FEE_SAT), hook)
            .await
        {
            Ok((tx, _)) => {
                spend = Some(tx);
                break;
            }
            Err(e) => {
                if attempt % 6 == 0 {
                    println!("[carol] exit output not spendable yet (attempt {attempt}): {e}");
                }
                regtest::mine(1)?;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    let spend = spend.ok_or_else(|| anyhow!("Carol could not spend the exited VTXO"))?;
    ensure!(spend.input.iter().any(|i| i.previous_output == carol_vtxo), "spend does not include Carol's RGB output");
    let mut spend_txid = None;
    for i in 0..60 {
        match regtest.send_raw(&spend) {
            Ok(t) => {
                spend_txid = Some(t);
                break;
            }
            Err(e) => {
                if i % 6 == 0 {
                    println!("[carol] broadcast attempt {i}: {e}");
                }
                regtest::mine(1)?;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    let spend_txid = spend_txid.ok_or_else(|| anyhow!("could not broadcast Carol->Dave"))?;
    rec("carol_to_dave_txid", spend_txid.to_string(), &mut summary);
    let cons_cd = state.join("carol_to_dave.rgbc");
    carol_transfers.ok_or_else(|| anyhow!("no transfers"))?[0].save_file(&cons_cd).map_err(|e| anyhow!("save: {e}"))?;
    let _down = if offline {
        // every witness is mined after Carol's exit: the proof is empty, Dave needs only the chain
        let st = crate::proof::build_proof(&carol, &regtest, &all_witnesses, &[], &crate::proof::proof_path(&cons_cd)).await?;
        rec("proof_cd", format!("{} B, {} PSBTs, {} witnesses", st.bytes, st.n_psbts, st.n_witnesses), &mut summary);
        Some(crate::proof::arkd_down(&mut |k: &str, v: String| rec(k, v, &mut summary), "dave")?)
    } else {
        None
    };
    let mut provided = None;
    for i in 0..10 {
        match bl(|| dave_rgb.provide_out_of_band_consignment(dave_online.clone(), cons_cd.display().to_string(), vec![])) {
            Ok(r) => {
                provided = Some(r);
                break;
            }
            Err(e) => {
                println!("[dave] provide_out_of_band_consignment attempt {i}: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    println!("[dave] provide_out_of_band_consignment: {:?}", provided.ok_or_else(|| anyhow!("dave could not accept"))?);
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let refreshed = bl(|| dave_rgb.refresh(dave_online.clone(), None, vec![], false))?;
    println!("[dave] refresh: {refreshed:?}");
    let assets = bl(|| dave_rgb.list_assets(vec![]))?;
    println!("[dave] list_assets: {:?}", assets.nia);
    let bal = bl(|| dave_rgb.get_asset_balance(asset_id.clone()))?;
    rec("dave_balance", format!("{bal:?}"), &mut summary);
    let pass = assets.nia.as_ref().is_some_and(|v| v.iter().any(|a| a.asset_id == asset_id)) && bal.settled == CAROL_ASSET;
    rec("PASS", pass.to_string(), &mut summary);
    println!("\nOFFCHAIN TRANSFER RESULT: {}", if pass { "PASS" } else { "FAIL" });
    if !pass {
        bail!("Dave balance check failed");
    }
    Ok(())
}

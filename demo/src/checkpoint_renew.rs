//! Scenario `checkpoint-renew`: RGB on an arkd with ONE change, a data OP_RETURN in the checkpoint
//! tx (profile `cponly`: arkd master + that change only). Standard RGB, no forfeit involved.
//!
//! The checkpoint is the tx that spends a VTXO off-chain, so with the commitment there every
//! transfer is plain RGB: the seal (VTXO) is spent by the tx that commits. The new seal is a VTXO
//! the receiver already holds, like an RGB invoice on an existing UTXO. Off-chain outputs inherit
//! the earliest expiry of their inputs, so renewal = moving the asset onto a VTXO of a later batch:
//!   1. asset issued on-chain and moved onto Alice's VTXO V (batch 1)
//!   2. later batch: Alice holds G, Bob holds H
//!   3. renew: Alice's checkpoint of V commits V -> G
//!   4. batch 1 expires and is swept
//!   5. send: the checkpoint of G commits G -> H; Bob validates offline (negative: unattested)
//!   6. arkd restarts
use crate::proof::{add_vtxos, arkd_down, build_proof, import, operators, proof_path};
use crate::{ArkClient, ark_client, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_client::GetVtxosRequestFilter;
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions};
use bitcoin::{OutPoint, ScriptBuf, Txid};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::{Outpoint, Wallet as RgbWallet};
use rgb_lib::{ContractId, FileContent, RgbTransfer};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

const UNITS: u64 = crate::ISSUE_AMOUNT;
const BL: u64 = 9101;

pub async fn run() -> anyhow::Result<()> {
    let run_id = format!("run-{}-checkpoint-renew", now());
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
    let first = |l: ark_client::AnnotatedVtxoList| l.spendable_offchain().next().map(|v| v.vtxo().outpoint);

    step("1. Alice boards (batch 1); the asset is issued on-chain and moved onto her VTXO V");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    rec("arkd_version", info.version.clone());
    board("alice", &alice, "0.001").await?;
    let v = first(alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?).ok_or_else(|| anyhow!("alice has no VTXO"))?;
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, v, &state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    rec("asset_on_v", format!("{asset_id} on {v}"));

    step("2. a later batch: Alice holds G (separate identity: settle would forfeit V), Bob holds H");
    regtest::mine(240)?;
    let (alice_g, _) = ark_client("alice-g", regtest.clone(), &state).await?;
    board("alice-g", &alice_g, "0.0005").await?;
    let g = first(alice_g.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?).ok_or_else(|| anyhow!("alice-g has no VTXO"))?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    board("bob", &bob, "0.0005").await?;
    let h = first(bob.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?).ok_or_else(|| anyhow!("bob has no VTXO"))?;
    let exp = |x: Option<ark_core::server::VirtualTxOutPoint>| x.map(|x| x.expires_at).unwrap_or_default();
    let (v_exp, g_exp) = (exp(crate::long_offline::vtxo_info(v).await), exp(crate::long_offline::vtxo_info(g).await));
    rec("expiry", format!("V {v} {v_exp}, G {g} {g_exp}, H {h}"));
    ensure!(g_exp > v_exp, "G is not fresher than V");

    step("3. renew: one Arkade tx, the checkpoint of V commits V -> G");
    let (cp1, _, renew) = checkpoint_hop(&alice, &alice_rgb, &info, v, cid, UNITS, g, BL).await?;
    rec("renew_checkpoint", cp1.to_string());
    rec("renew_ark_tx", renew.to_string());

    step("4. batch 1 expires (mine past its tree expiry) and arkd sweeps it");
    regtest::mine(270)?;
    let left = OutPoint { txid: renew, vout: 0 };
    let mut swept = false;
    for _ in 0..40 {
        if crate::long_offline::vtxo_info(left).await.is_some_and(|x| x.is_swept) {
            swept = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    rec("batch1_swept", format!("{left} (bitcoin left on batch 1) swept: {swept}"));

    step("5. send: the checkpoint of G commits G -> H (Bob's VTXO); Bob validates offline");
    let (cp2, transfer, send) = checkpoint_hop(&alice_g, &alice_rgb, &info, g, cid, UNITS, h, BL + 1).await?;
    rec("send_checkpoint", cp2.to_string());
    rec("send_ark_tx", send.to_string());
    let consignment = state.join("g-to-h.rgbc");
    transfer.save_file(&consignment).map_err(|e| anyhow!("save consignment: {e}"))?;
    let r = bl(|| bob_rgb.validate_consignment_ark(bob_online.clone(), consignment.clone(), true));
    rec("neg_unattested_checkpoint_rejected", r.is_err().to_string());
    ensure!(r.is_err(), "consignment accepted before its checkpoints were attested");
    let pp = proof_path(&consignment);
    build_proof(&alice_g, &regtest, &[cp1, cp2], &[], &pp).await?;
    add_vtxos(&bob, &regtest, &pp, &[h]).await?;
    let ops = operators(&info);
    {
        let _down = arkd_down(&mut rec, "bob")?;
        let reg = import(&bob_rgb, &bob_online, &pp, &ops)?;
        rec("bob_proof_registered", format!("{reg:?}"));
        let got = bl(|| bob_rgb.ark_receive_vtxo_transfer(bob_online.clone(), consignment.clone(), h.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("bob_received_on_h", got.to_string());
        ensure!(got == UNITS, "Bob received {got}, expected {UNITS}");
    }

    step("6. arkd restarts with these checkpoints stored");
    let restarted = std::process::Command::new("docker").args(["restart", "arkd"]).status()?.success();
    let mut up = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if crate::long_offline::vtxo_info(h).await.is_some() {
            up = true;
            break;
        }
    }
    rec("arkd_restart", format!("docker restart ok {restarted}, serving again within 90 s: {up}"));
    ensure!(up, "arkd did not come back");
    println!("CHECKPOINT RENEW RESULT: PASS");
    Ok(())
}

/// Spend the colored VTXO `from` off-chain (its bitcoin back to the owner) with the commitment in
/// its checkpoint: all `units` move to the existing VTXO `to`. Returns (checkpoint, transfer, ark tx).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn checkpoint_hop(
    ark: &ArkClient,
    rgb: &RgbWallet,
    info: &ark_core::server::Info,
    from: OutPoint,
    cid: ContractId,
    units: u64,
    to: OutPoint,
    blinding: u64,
) -> anyhow::Result<(Txid, RgbTransfer, Txid)> {
    let input = ark.resolve_selected_send_inputs(&[from]).await?.remove(0);
    let (me, me_vtxo) = ark.get_offchain_address().await?;
    let receivers = vec![SendReceiver::bitcoin(me.clone(), input.amount())];
    // two-pass build: the commitment does not depend on the checkpoint txid (explicit seal)
    let dry = build_offchain_transactions(&receivers, &me, &[input.clone().with_checkpoint_op_return(Some([0u8; 32]))], info)?;
    let mut cp = dry.checkpoint_txs[0].clone();
    cp.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    let seal = SealTarget::Explicit(Outpoint { txid: to.txid.to_string(), vout: to.vout });
    let transfer = bl(|| rgb.color_psbt_and_consume_custom(&mut cp, cid, vec![(seal, units, blinding)]))?;
    let commitment = cp.unsigned_tx.output[0].script_pubkey.clone();
    ensure!(commitment.is_op_return() && commitment.len() == 34, "bad commitment {commitment}");
    let mut payload = [0u8; 32];
    payload.copy_from_slice(&commitment.as_bytes()[2..]);
    let OffchainTransactions { ark_tx, checkpoint_txs } =
        build_offchain_transactions(&receivers, &me, &[input.with_checkpoint_op_return(Some(payload))], info)?;
    let cp_txid = checkpoint_txs[0].unsigned_tx.compute_txid();
    ensure!(cp_txid == cp.unsigned_tx.compute_txid(), "checkpoint changed between the two builds");
    let ark_txid = ark_tx.unsigned_tx.compute_txid();
    let pending = ark.submit_built_offchain_send(ark_tx, checkpoint_txs, me_vtxo.owner_pk()).await?;
    ark.sign_and_finalize_pending_tx(pending).await?;
    Ok((cp_txid, transfer, ark_txid))
}

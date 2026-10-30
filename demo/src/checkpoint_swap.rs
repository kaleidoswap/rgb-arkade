//! Scenario `checkpoint-swap`: with ONLY the checkpoint data OP_RETURN in arkd (profile `cponly`),
//! an atomic asset-for-bitcoin swap, then a send to someone who holds no VTXO yet. Standard RGB.
//!
//! Every RGB transition sits in the seller's checkpoint and pays existing VTXOs (explicit seals),
//! so the ark tx carries no commitment:
//!   swap:  checkpoint(S): S -> H (buyer, 400) + C (seller's change, 600)   checkpoint(B): nothing
//!          ark tx: [seller: S sat + price] [buyer: bitcoin change]
//!   1. the seller's transition short-changes the buyer: the buyer refuses before signing
//!   2. the swap; the buyer validates offline (arkd paused) and receives 400 on H
//!   3. witness-like send: Bob pays Carol (no VTXO) plain bitcoin, which creates her VTXO R,
//!      then moves the 400 units onto R with a checkpoint commitment; Carol validates offline
use crate::checkpoint_renew::checkpoint_hop;
use crate::offchain_transfer::wait_vtxo;
use crate::proof::{add_vtxos, arkd_down, build_proof, import, operators, proof_path};
use crate::swap::{Side, complete};
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_client::GetVtxosRequestFilter;
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions};
use bitcoin::{Amount, OutPoint, Psbt, ScriptBuf, Txid};
use regtest::Regtest;
use rgb_lib::wallet::Outpoint;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::{Assignment, ContractId, FileContent};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

const SOLD: u64 = 400;
const PRICE_SAT: u64 = 30_000;
const CAROL_SAT: u64 = 1_000;

struct Prepared {
    ark_tx: Psbt,
    checkpoints: Vec<Psbt>,
    seller_cp: Txid,
    ark_txid: Txid,
    buyer_change: u64,
}

fn explicit(o: OutPoint) -> SealTarget {
    SealTarget::Explicit(Outpoint { txid: o.txid.to_string(), vout: o.vout })
}

/// Seller: build checkpoint(S) + checkpoint(B) + the ark tx and color the seller's checkpoint:
/// `to_buyer` units onto the buyer's VTXO `h`, the rest onto the seller's VTXO `c`.
#[allow(clippy::too_many_arguments)]
async fn prepare(seller: &Side<'_>, buyer: &Side<'_>, cid: ContractId, to_buyer: u64, h: OutPoint, c: OutPoint, blinding: u64, consignment: &Path) -> anyhow::Result<Prepared> {
    let info = seller.ark.server_info().await?;
    let s_in = seller.ark.resolve_selected_send_inputs(&[seller.vtxo]).await?.remove(0);
    let b_in = buyer.ark.resolve_selected_send_inputs(&[buyer.vtxo]).await?.remove(0);
    let (s_sat, b_sat) = (s_in.amount().to_sat(), b_in.amount().to_sat());
    ensure!(b_sat >= PRICE_SAT, "buyer VTXO {b_sat} sat < price {PRICE_SAT}");
    let (seller_addr, _) = seller.ark.get_offchain_address().await?;
    let (buyer_addr, _) = buyer.ark.get_offchain_address().await?;
    let mut receivers = vec![SendReceiver::bitcoin(seller_addr.clone(), Amount::from_sat(s_sat + PRICE_SAT))];
    if b_sat > PRICE_SAT {
        receivers.push(SendReceiver::bitcoin(buyer_addr, Amount::from_sat(b_sat - PRICE_SAT)));
    }

    let dry = build_offchain_transactions(&receivers, &seller_addr, &[s_in.clone().with_checkpoint_op_return(Some([0u8; 32])), b_in.clone()], &info)?;
    let mut cp = dry.checkpoint_txs[0].clone();
    cp.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    let beneficiaries = vec![(explicit(h), to_buyer, blinding), (explicit(c), ISSUE_AMOUNT - to_buyer, blinding + 1)];
    let transfer = bl(|| seller.rgb.color_psbt_and_consume_custom(&mut cp, cid, beneficiaries))?;
    transfer.save_file(consignment).map_err(|e| anyhow!("save consignment: {e}"))?;
    let commitment = cp.unsigned_tx.output[0].script_pubkey.clone();
    ensure!(commitment.is_op_return() && commitment.len() == 34, "bad commitment {commitment}");
    let mut payload = [0u8; 32];
    payload.copy_from_slice(&commitment.as_bytes()[2..]);

    let OffchainTransactions { ark_tx, checkpoint_txs } =
        build_offchain_transactions(&receivers, &seller_addr, &[s_in.with_checkpoint_op_return(Some(payload)), b_in], &info)?;
    let seller_cp = checkpoint_txs[0].unsigned_tx.compute_txid();
    ensure!(seller_cp == cp.unsigned_tx.compute_txid(), "seller checkpoint changed between the two builds");
    ensure!(ark_tx.unsigned_tx.output.iter().all(|o| !o.script_pubkey.is_op_return() || o.value > Amount::ZERO), "ark tx carries a data OP_RETURN");
    let ark_txid = ark_tx.unsigned_tx.compute_txid();
    Ok(Prepared { ark_tx, checkpoints: checkpoint_txs, seller_cp, ark_txid, buyer_change: b_sat - PRICE_SAT })
}

/// Buyer: before signing, the bitcoin change is right and the still-unsigned seller checkpoint
/// gives `SOLD` units on the buyer's VTXO `h`.
fn buyer_checks(buyer: &Side<'_>, p: &Prepared, h: OutPoint, consignment: &Path) -> anyhow::Result<()> {
    let o = &p.ark_tx.unsigned_tx.output;
    if p.buyer_change > 0 {
        ensure!(o.iter().any(|x| x.value.to_sat() == p.buyer_change), "no bitcoin change of {} sat", p.buyer_change);
    }
    let got = bl(|| buyer.rgb.validate_consignment_offchain(buyer.online.clone(), consignment.to_path_buf(), vec![p.seller_cp.to_string()], h.txid.to_string(), h.vout))?;
    let units: u64 = got.iter().map(|a| if let Assignment::Fungible(n) = a { *n } else { 0 }).sum();
    ensure!(units == SOLD, "consignment gives {units} on {h}, agreed {SOLD}");
    println!("[{}] checked before signing: {units} units on {h}, change ok", buyer.name);
    Ok(())
}

async fn first(ark: &ArkClient, who: &str) -> anyhow::Result<OutPoint> {
    ark.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().next().map(|v| v.vtxo().outpoint).ok_or_else(|| anyhow!("{who} has no VTXO"))
}

pub async fn run() -> anyhow::Result<()> {
    let run_id = format!("run-{}-checkpoint-swap", now());
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

    step("1. Alice: asset on S, spare VTXO C for the change. Bob: bitcoin on B, spare VTXO H to receive");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (alice_c, _) = ark_client("alice-c", regtest.clone(), &state).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    let (bob_h, _) = ark_client("bob-h", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    rec("arkd_version", info.version.clone());
    board("alice", &alice, "0.001").await?;
    board("alice-c", &alice_c, "0.0002").await?;
    board("bob", &bob, "0.001").await?;
    board("bob-h", &bob_h, "0.0002").await?;
    let (s, c, b, h) = (first(&alice, "alice").await?, first(&alice_c, "alice-c").await?, first(&bob, "bob").await?, first(&bob_h, "bob-h").await?);
    rec("vtxos", format!("S {s}, C {c}, B {b}, H {h}"));
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;
    let (mut carol_rgb, carol_online) = bl(|| rgb_wallet("carol", &state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, s, &state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    rec("asset_on_s", format!("{asset_id} on {s}"));

    let seller = Side { name: "alice", ark: &alice, rgb: &alice_rgb, online: &alice_online, vtxo: s };
    let buyer = Side { name: "bob", ark: &bob, rgb: &bob_rgb, online: &bob_online, vtxo: b };

    step("2. negative: Alice's checkpoint gives Bob 300 instead of 400; Bob refuses before signing");
    let bad = state.join("short.rgbc");
    let p = prepare(&seller, &buyer, cid, SOLD - 100, h, c, 8001, &bad).await?;
    let refused = buyer_checks(&buyer, &p, h, &bad);
    rec("neg_shortchange_refused", format!("{:?}", refused.as_ref().err().map(|e| e.to_string())));
    ensure!(refused.is_err(), "Bob accepted a short-changed swap");
    bl(|| alice_rgb.ark_rollback_transfer(alice_online.clone(), p.seller_cp.to_string()))?;

    step("3. the swap: 400 units for 30,000 sat in one Arkade tx, commitment only in Alice's checkpoint");
    let consignment = state.join("swap.rgbc");
    let p = prepare(&seller, &buyer, cid, SOLD, h, c, 8011, &consignment).await?;
    buyer_checks(&buyer, &p, h, &consignment)?;
    let buyer_cp = complete(&seller, &buyer, p.ark_tx.clone(), p.checkpoints.clone(), p.ark_txid).await?;
    rec("seller_checkpoint", p.seller_cp.to_string());
    rec("buyer_checkpoint", buyer_cp.to_string());
    rec("ark_txid", p.ark_txid.to_string());
    wait_vtxo(&alice, OutPoint { txid: p.ark_txid, vout: 0 }, "alice").await?;
    let alice_sat = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().find(|v| v.vtxo().outpoint.txid == p.ark_txid).map(|v| v.vtxo().amount.to_sat()).unwrap_or(0);
    rec("alice_paid_vtxo_sat", alice_sat.to_string());
    ensure!(alice_sat >= PRICE_SAT, "Alice's new VTXO holds {alice_sat} sat");

    step("4. Bob validates with arkd paused and receives 400 on H; Alice's 600 change is on C");
    let pp = proof_path(&consignment);
    build_proof(&alice, &regtest, &[p.seller_cp], &[], &pp).await?;
    add_vtxos(&bob_h, &regtest, &pp, &[h, c]).await?;
    let ops = operators(&info);
    {
        let _down = arkd_down(&mut rec, "bob")?;
        let reg = import(&bob_rgb, &bob_online, &pp, &ops)?;
        rec("bob_proof_registered", format!("{reg:?}"));
        let got = bl(|| bob_rgb.ark_receive_vtxo_transfer(bob_online.clone(), consignment.clone(), h.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("bob_received_on_h", got.to_string());
        ensure!(got == SOLD, "Bob received {got}, expected {SOLD}");
        import(&alice_rgb, &alice_online, &pp, &ops)?;
        let change = bl(|| alice_rgb.validate_consignment_offchain(alice_online.clone(), consignment.clone(), vec![], c.txid.to_string(), c.vout))?;
        let change: u64 = change.iter().map(|a| if let Assignment::Fungible(n) = a { *n } else { 0 }).sum();
        rec("alice_change_on_c", change.to_string());
        ensure!(change == ISSUE_AMOUNT - SOLD, "Alice's change {change}");
    }

    step("5. witness-like send: Bob pays Carol (no VTXO) bitcoin, creating her VTXO R");
    let (carol, _) = ark_client("carol", regtest.clone(), &state).await?;
    let (carol_addr, _) = carol.get_offchain_address().await?;
    let pay = bob.send(vec![SendReceiver::bitcoin(carol_addr, Amount::from_sat(CAROL_SAT))]).await?;
    let mut r = None;
    for _ in 0..20 {
        r = carol.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().find(|v| v.vtxo().outpoint.txid == pay).map(|v| v.vtxo().outpoint);
        if r.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    let r = r.ok_or_else(|| anyhow!("Carol does not see the VTXO from {pay}"))?;
    rec("carol_vtxo_r", r.to_string());

    step("6. Bob moves the 400 units from H onto R; Carol validates offline");
    let (cp3, transfer, hop) = checkpoint_hop(&bob_h, &bob_rgb, &info, h, cid, SOLD, r, 8021).await?;
    rec("hop_checkpoint", cp3.to_string());
    rec("hop_ark_tx", hop.to_string());
    let to_carol: PathBuf = state.join("h-to-r.rgbc");
    transfer.save_file(&to_carol).map_err(|e| anyhow!("save consignment: {e}"))?;
    let pp2 = proof_path(&to_carol);
    build_proof(&bob_h, &regtest, &[p.seller_cp, cp3], &[], &pp2).await?;
    add_vtxos(&carol, &regtest, &pp2, &[r]).await?;
    {
        let _down = arkd_down(&mut rec, "carol")?;
        let reg = import(&carol_rgb, &carol_online, &pp2, &ops)?;
        rec("carol_proof_registered", format!("{reg:?}"));
        let got = bl(|| carol_rgb.ark_receive_vtxo_transfer(carol_online.clone(), to_carol.clone(), r.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("carol_received_on_r", got.to_string());
        ensure!(got == SOLD, "Carol received {got}, expected {SOLD}");
    }
    println!("CHECKPOINT SWAP RESULT: PASS");
    Ok(())
}

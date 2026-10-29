//! Scenario `swap`: atomic RGB-asset-for-bitcoin swap in a single Arkade tx (requires `arkd:rgb`).
//!
//! The seller holds an asset on VTXO S, the buyer a bitcoin-only VTXO B. One Arkade tx spends both:
//!   checkpoint(S): T1, S seal -> CP_S:1        checkpoint(B): no commitment
//!   ark tx: [OP_RETURN T2] [buyer: asset carrier] [seller: price + asset change] [buyer: bitcoin change]
//! Each party signs only its own inputs. The buyer validates the asset (consignment over the
//! still-unsigned txs) before signing, so both legs happen in the same tx or neither does.
//!   1. the buyer refuses after the seller prepared T1/T2: the seller rolls back, asset intact
//!   2. the swap: the buyer validates offline (arkd paused) from the consignment + Ark proof
use crate::batch_scenarios::own;
use crate::offchain_transfer::{outs, wait_vtxo};
use crate::proof::{add_vtxos, arkd_down, build_proof, import, operators, proof_path};
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bare_opret, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_client::GetVtxosRequestFilter;
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions};
use bitcoin::{Amount, OutPoint, Psbt, ScriptBuf, Txid, psbt};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::{Online, Wallet as RgbWallet};
use rgb_lib::{Assignment, ContractId, FileContent, RgbTransfer};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

/// One side of a swap: its Ark client, RGB wallet and the VTXO it puts in.
pub(crate) struct Side<'a> {
    pub(crate) name: &'a str,
    pub(crate) ark: &'a ArkClient,
    pub(crate) rgb: &'a RgbWallet,
    pub(crate) online: &'a Online,
    pub(crate) vtxo: OutPoint,
}

#[derive(Clone, Copy)]
pub(crate) struct Terms {
    pub(crate) contract_id: ContractId,
    /// asset units the buyer gets; the rest stays with the seller (asset change)
    pub(crate) asset: u64,
    /// sat the buyer pays
    pub(crate) price: u64,
    /// sat on the buyer's new asset VTXO (taken from the seller's VTXO)
    pub(crate) carrier: u64,
    pub(crate) blinding_buyer: u64,
    pub(crate) blinding_change: u64,
    pub(crate) blinding_cp: u64,
}

pub(crate) struct Swap {
    pub(crate) seller_checkpoint: Txid,
    pub(crate) buyer_checkpoint: Txid,
    pub(crate) ark_txid: Txid,
    pub(crate) consignment: PathBuf,
}

/// The joint txs and the seller's RGB transitions, nothing signed yet.
struct Prepared {
    ark_tx: Psbt,
    checkpoints: Vec<Psbt>,
    ark_txid: Txid,
    seller_vtxo_sat: u64,
    buyer_vtxo_sat: u64,
}

/// Seller: build checkpoint(S) + checkpoint(B) + the ark tx, color T1 (checkpoint) and T2 (ark tx).
async fn prepare(seller: &Side<'_>, buyer: &Side<'_>, t: &Terms, consignment: &PathBuf) -> anyhow::Result<Prepared> {
    let info = seller.ark.server_info().await?;
    let s_in = seller.ark.resolve_selected_send_inputs(&[seller.vtxo]).await?;
    let b_in = buyer.ark.resolve_selected_send_inputs(&[buyer.vtxo]).await?;
    ensure!(s_in.len() == 1 && b_in.len() == 1, "expected one VTXO per side");
    let (s_sat, b_sat) = (s_in[0].amount().to_sat(), b_in[0].amount().to_sat());
    ensure!(t.carrier < s_sat && t.price <= b_sat, "seller VTXO {s_sat} sat, buyer VTXO {b_sat} sat: terms do not fit");
    let (buyer_addr, _) = buyer.ark.get_offchain_address().await?;
    let (seller_addr, _) = seller.ark.get_offchain_address().await?;
    let (buyer_change, _) = buyer.ark.get_offchain_address().await?;
    // exact outputs: no change output, so every vout's owner is fixed
    let mut receivers = vec![
        SendReceiver::bitcoin(buyer_addr, Amount::from_sat(t.carrier)),
        SendReceiver::bitcoin(seller_addr, Amount::from_sat(s_sat - t.carrier + t.price)),
    ];
    if b_sat > t.price {
        receivers.push(SendReceiver::bitcoin(buyer_change, Amount::from_sat(b_sat - t.price)));
    }

    // T1 in the seller's checkpoint: S seal -> CP_S:1 (same two-pass build as rgb_hop)
    let dry_inputs = vec![s_in[0].clone().with_checkpoint_op_return(Some([0u8; 32])), b_in[0].clone()];
    let dry = build_offchain_transactions(&receivers, &seller_addr, &dry_inputs, &info)?;
    let mut cp_colored = dry.checkpoint_txs[0].clone();
    cp_colored.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    let owned = asset_on(seller.rgb, seller.online, &t.contract_id, seller.vtxo)?;
    ensure!(owned >= t.asset, "{} holds {owned} on {}, cannot sell {}", seller.name, seller.vtxo, t.asset);
    bl(|| seller.rgb.color_psbt_and_consume_custom(&mut cp_colored, t.contract_id, vec![(SealTarget::WitnessVout(1), owned, t.blinding_cp)]))?;
    let commitment = cp_colored.unsigned_tx.output[0].script_pubkey.clone();
    ensure!(commitment.is_op_return() && commitment.len() == 34, "bad T1 commitment {commitment}");
    let mut payload = [0u8; 32];
    payload.copy_from_slice(&commitment.as_bytes()[2..]);

    let inputs = vec![s_in[0].clone().with_checkpoint_op_return(Some(payload)), b_in[0].clone()];
    let OffchainTransactions { mut ark_tx, checkpoint_txs } = build_offchain_transactions(&receivers, &seller_addr, &inputs, &info)?;
    ensure!(
        checkpoint_txs[0].unsigned_tx.compute_txid() == cp_colored.unsigned_tx.compute_txid(),
        "seller checkpoint changed between the two builds"
    );
    ensure!(ark_tx.unsigned_tx.input.len() == 2, "ark tx should spend both checkpoints");

    // T2 in the ark tx: CP_S:1 -> buyer (vout 1) + asset change to the seller (vout 2)
    ark_tx.unsigned_tx.output.insert(0, bare_opret(Amount::ZERO));
    ark_tx.outputs.insert(0, psbt::Output::default());
    let mut beneficiaries = vec![(SealTarget::WitnessVout(1), t.asset, t.blinding_buyer)];
    if owned > t.asset {
        beneficiaries.push((SealTarget::WitnessVout(2), owned - t.asset, t.blinding_change));
    }
    let mut ark_colored = ark_tx.clone();
    let t2: RgbTransfer = bl(|| seller.rgb.color_psbt_and_consume_custom(&mut ark_colored, t.contract_id, beneficiaries))?;
    ark_tx.unsigned_tx.output[0].script_pubkey = ark_colored.unsigned_tx.output[0].script_pubkey.clone();
    let ark_txid = ark_tx.unsigned_tx.compute_txid();
    ensure!(ark_txid == ark_colored.unsigned_tx.compute_txid(), "ark txid changed after the commitment");
    t2.save_file(consignment).map_err(|e| anyhow!("save consignment: {e}"))?;
    println!("[swap] ark tx {ark_txid} outputs {:?}", outs(&ark_tx));
    Ok(Prepared { ark_tx, checkpoints: checkpoint_txs, ark_txid, seller_vtxo_sat: s_sat, buyer_vtxo_sat: b_sat })
}

/// Asset units of `cid` on the wallet's active VTXO `o` (rgb-lib's Ark registry, from the chain).
fn asset_on(rgb: &RgbWallet, online: &Online, cid: &ContractId, o: OutPoint) -> anyhow::Result<u64> {
    let b = bl(|| rgb.get_asset_balance_ark(online.clone(), cid.to_string()))?;
    Ok(b.vtxos
        .iter()
        .filter(|(op, _, st)| *op == o.to_string() && *st == rgb_lib::ark_proof::ArkVtxoStatus::Active)
        .map(|(_, n, _)| n)
        .sum())
}

/// Buyer: before signing, check the ark tx pays what was agreed and the consignment gives the asset.
fn buyer_checks(buyer: &Side<'_>, p: &Prepared, t: &Terms, consignment: &PathBuf) -> anyhow::Result<()> {
    let o = &p.ark_tx.unsigned_tx.output;
    ensure!(o[0].script_pubkey.is_op_return() && o[0].value == Amount::ZERO, "no RGB commitment at vout 0");
    ensure!(o[1].value.to_sat() == t.carrier, "asset output carries {} sat, agreed {}", o[1].value, t.carrier);
    if p.buyer_vtxo_sat > t.price {
        ensure!(o[3].value.to_sat() == p.buyer_vtxo_sat - t.price, "bitcoin change {} != {}", o[3].value, p.buyer_vtxo_sat - t.price);
    }
    let witnesses = vec![p.checkpoints[0].unsigned_tx.compute_txid().to_string(), p.ark_txid.to_string()];
    let got = bl(|| buyer.rgb.validate_consignment_offchain(buyer.online.clone(), consignment.clone(), witnesses, p.ark_txid.to_string(), 1))?;
    let units: u64 = got.iter().map(|a| if let Assignment::Fungible(n) = a { *n } else { 0 }).sum();
    ensure!(units == t.asset, "consignment gives {units} on vout 1, agreed {}", t.asset);
    println!("[{}] checked before signing: {units} units on ark tx vout 1, {} sat carrier, change ok", buyer.name, t.carrier);
    Ok(())
}

#[derive(PartialEq)]
pub(crate) enum Case {
    Agreed,
    /// the buyer walks away after the seller prepared its side
    BuyerWalksAway,
    /// the seller's transition gives the buyer 100 units less than agreed
    SellerShortchanges,
}

/// The whole swap. Unless both sides do what was agreed, nothing is submitted and the seller
/// rolls its transitions back.
pub(crate) async fn atomic_swap(seller: &Side<'_>, buyer: &Side<'_>, t: &Terms, consignment: PathBuf, case: Case) -> anyhow::Result<Option<Swap>> {
    let short = Terms { asset: t.asset - 100, ..*t };
    let mut p = prepare(seller, buyer, if case == Case::SellerShortchanges { &short } else { t }, &consignment).await?;
    let seller_cp = p.checkpoints[0].unsigned_tx.compute_txid();
    let refused = match case {
        Case::BuyerWalksAway => Some("walks away".to_string()),
        _ => buyer_checks(buyer, &p, t, &consignment).err().map(|e| format!("refuses: {e:#}")),
    };
    if let Some(why) = refused {
        println!("[{}] {why}: nothing reaches arkd", buyer.name);
        for w in [p.ark_txid, seller_cp] {
            bl(|| seller.rgb.ark_rollback_transfer(seller.online.clone(), w.to_string()))?;
        }
        println!("[{}] rolled back T2 {} and T1 {seller_cp}", seller.name, p.ark_txid);
        return Ok(None);
    }

    // The buyer signs everything it owns up front (its ark tx input and its checkpoint) and is
    // done: the seller completes alone, so the buyer holds no option to finish later or not.
    // Checkpoint signatures do not depend on the server's (taproot, SIGHASH_DEFAULT).
    buyer.ark.sign_ark_tx_inputs(&mut p.ark_tx, &[1]).map_err(|e| anyhow!("buyer sign: {e:?}"))?;
    let mut buyer_cp_signed = p.checkpoints[1].clone();
    if buyer_cp_signed.inputs[0].witness_script.is_none() {
        buyer_cp_signed.inputs[0].witness_script = p.ark_tx.inputs[1].witness_script.clone();
    }
    buyer.ark.sign_checkpoint_tx(&mut buyer_cp_signed).map_err(|e| anyhow!("buyer checkpoint sign: {e:?}"))?;
    ensure!(!buyer_cp_signed.inputs[0].tap_script_sigs.is_empty(), "the buyer could not pre-sign its checkpoint");
    println!("[{}] signed its ark tx input and its checkpoint, hands them over", buyer.name);

    seller.ark.sign_ark_tx_inputs(&mut p.ark_tx, &[0]).map_err(|e| anyhow!("seller sign: {e:?}"))?;
    let res = seller
        .ark
        .network_client()
        .submit_offchain_transaction_request(p.ark_tx.clone(), p.checkpoints.clone())
        .await
        .map_err(|e| anyhow!("submit: {e:?}"))?;
    ensure!(res.signed_ark_tx.unsigned_tx.compute_txid() == p.ark_txid, "arkd rebuilt a different ark tx");

    // the server co-signed both checkpoints: the seller signs its own and adds the buyer's
    // signature from before the submit; the buyer takes no part from here on
    let mut cps = res.signed_checkpoint_txs;
    let buyer_cp = buyer_cp_signed.unsigned_tx.compute_txid();
    for cp in cps.iter_mut() {
        let txid = cp.unsigned_tx.compute_txid();
        let idx = res.signed_ark_tx.unsigned_tx.input.iter().position(|i| i.previous_output.txid == txid).ok_or_else(|| anyhow!("checkpoint {txid} not spent by the ark tx"))?;
        if cp.inputs[0].witness_script.is_none() {
            cp.inputs[0].witness_script = res.signed_ark_tx.inputs[idx].witness_script.clone();
        }
        if txid == buyer_cp {
            cp.inputs[0].tap_script_sigs.extend(buyer_cp_signed.inputs[0].tap_script_sigs.clone());
        } else {
            seller.ark.sign_checkpoint_tx(cp).map_err(|e| anyhow!("seller checkpoint sign: {e:?}"))?;
        }
    }
    seller.ark.finalize_offchain_tx(p.ark_txid, cps).await.map_err(|e| anyhow!("finalize: {e:?}"))?;
    println!(
        "[swap] final: seller VTXO {} sat -> {} sat + asset change, buyer VTXO {} sat -> asset carrier {} sat + change",
        p.seller_vtxo_sat,
        p.seller_vtxo_sat - t.carrier + t.price,
        p.buyer_vtxo_sat,
        t.carrier
    );
    Ok(Some(Swap { seller_checkpoint: seller_cp, buyer_checkpoint: buyer_cp, ark_txid: p.ark_txid, consignment }))
}

const ALICE_SELLS: u64 = 400;
const PRICE_SAT: u64 = 30_000;
const CARRIER_SAT: u64 = 1_000;

pub async fn run() -> anyhow::Result<()> {
    let run_id = format!("run-{}-swap", now());
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

    step("1. Alice (asset) and Bob (bitcoin) board");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    board("alice", &alice, "0.001").await?;
    board("bob", &bob, "0.001").await?;
    let av = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().next().map(|v| v.vtxo().outpoint).ok_or_else(|| anyhow!("alice has no VTXO"))?;
    let bv = bob.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().next().map(|v| v.vtxo().outpoint).ok_or_else(|| anyhow!("bob has no VTXO"))?;
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;

    step("2. Alice issues 1000 units and jumps them onto her VTXO");
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, av, &state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    rec("asset_id", asset_id.clone());
    let contract_id = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    let p = state.join("track-alice.arkproof.json");
    add_vtxos(&alice, &regtest, &p, &[av]).await?;
    let ops = operators(&alice.server_info().await?);
    import(&alice_rgb, &alice_online, &p, &ops)?;
    own(&alice_rgb, av)?;

    let terms = |bl_cp| Terms {
        contract_id,
        asset: ALICE_SELLS,
        price: PRICE_SAT,
        carrier: CARRIER_SAT,
        blinding_buyer: 7101,
        blinding_change: 7102,
        blinding_cp: bl_cp,
    };
    let seller = Side { name: "alice", ark: &alice, rgb: &alice_rgb, online: &alice_online, vtxo: av };
    let buyer = Side { name: "bob", ark: &bob, rgb: &bob_rgb, online: &bob_online, vtxo: bv };

    for (i, (case, label)) in [(Case::BuyerWalksAway, "Bob walks away after Alice prepared her side"), (Case::SellerShortchanges, "Alice's transition gives Bob 300 instead of 400")].into_iter().enumerate() {
        step(&format!("3{}. negative: {label}", ["a", "b"][i]));
        let none = atomic_swap(&seller, &buyer, &terms(7001 + i as u64), state.join(format!("refused-{i}.rgbc")), case).await?;
        ensure!(none.is_none(), "refused swap returned a result");
        let left = asset_on(&alice_rgb, &alice_online, &contract_id, av)?;
        rec(&format!("alice_asset_after_rollback_{i}"), left.to_string());
        ensure!(left == ISSUE_AMOUNT, "after the rollback Alice holds {left} on {av}, expected {ISSUE_AMOUNT}");
    }

    step("4. the swap: 400 units for 30,000 sat in one Arkade tx, Bob signs up front");
    let s = atomic_swap(&seller, &buyer, &terms(7009), state.join("swap.rgbc"), Case::Agreed).await?.ok_or_else(|| anyhow!("swap did not run"))?;
    rec("seller_checkpoint", s.seller_checkpoint.to_string());
    rec("buyer_checkpoint", s.buyer_checkpoint.to_string());
    rec("ark_txid", s.ark_txid.to_string());
    let bob_asset_vtxo = OutPoint { txid: s.ark_txid, vout: 1 };
    let alice_vtxo = OutPoint { txid: s.ark_txid, vout: 2 };
    wait_vtxo(&bob, bob_asset_vtxo, "bob").await?;
    wait_vtxo(&alice, alice_vtxo, "alice").await?;

    step("5. Bob validates with arkd paused: consignment + Ark proof + bitcoin chain");
    let pp = proof_path(&s.consignment);
    build_proof(&alice, &regtest, &[s.seller_checkpoint, s.ark_txid], &[], &pp).await?;
    add_vtxos(&alice, &regtest, &pp, &[bob_asset_vtxo, alice_vtxo]).await?;
    {
        let _down = arkd_down(&mut rec, "bob")?;
        let reg = import(&bob_rgb, &bob_online, &pp, &ops)?;
        rec("bob_proof_registered", format!("{reg:?}"));
        let mut bob_rgb = bob_rgb;
        let (_, got) = bl(|| bob_rgb.accept_transfer_consignment_offchain(bob_online.clone(), s.consignment.clone(), vec![], s.ark_txid.to_string(), 1, 7101))?;
        rec("bob_accepted", format!("{got:?}"));
        let units: u64 = got.iter().map(|a| if let Assignment::Fungible(n) = a { *n } else { 0 }).sum();
        ensure!(units == ALICE_SELLS, "Bob accepted {units}, expected {ALICE_SELLS}");
    }
    import(&alice_rgb, &alice_online, &pp, &ops)?;
    own(&alice_rgb, alice_vtxo)?;

    step("6. balances");
    let a_bal = bl(|| alice_rgb.get_asset_balance_ark(alice_online.clone(), asset_id.clone()))?;
    rec("alice_ark_balance", format!("active {} to_recover {} in_flight {}", a_bal.active, a_bal.to_recover, a_bal.in_flight));
    ensure!(a_bal.active == ISSUE_AMOUNT - ALICE_SELLS, "Alice active {} != {}", a_bal.active, ISSUE_AMOUNT - ALICE_SELLS);
    let alice_btc = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?;
    let alice_sat = alice_btc.spendable_offchain().find(|v| v.vtxo().outpoint == alice_vtxo).map(|v| v.vtxo().amount.to_sat()).unwrap_or(0);
    rec("alice_vtxo_sat", alice_sat.to_string());
    ensure!(alice_sat >= PRICE_SAT, "Alice's new VTXO holds {alice_sat} sat, the price was {PRICE_SAT}");
    println!("SWAP RESULT: PASS");
    Ok(())
}

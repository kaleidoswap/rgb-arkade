//! Scenario `stock-lane`: what RGB on Arkade can do on a STOCK arkd (no RGB patches), and where it stops.
//!
//! Stock arkd takes `OP_RETURN <32 bytes>` (the only shape an RGB opret can have) as a sub-dust
//! VTXO: its value must be at least vtxoMinAmount and below dust. Its checkpoints carry nothing.
//! So an asset can only sit on the *predicted* checkpoint output of a VTXO (jump-in) and move
//! once, with the commitment in the ark tx:
//!   1. jump-in: asset issued on-chain and moved onto cp(S):0, the checkpoint of Alice's next spend
//!   2. swap in one Arkade tx: Alice's asset for Bob's bitcoin, T only in the ark tx (1-sat opret);
//!      Bob checks before signing and signs up front, then validates offline (arkd paused)
//!   3. limits, recorded: a second hop (needs a commitment in the checkpoint); whether arkd indexes
//!      the opret as a VTXO; whether arkd still restarts afterwards
//!
//! One hop + unilateral exit + on-chain spend on stock arkd is scenario `basic`.
use crate::proof::{arkd_down, build_proof, import, operators, proof_path};
use crate::swap::{Side, complete};
use crate::{ISSUE_AMOUNT, ark_client, bare_opret, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_client::GetVtxosRequestFilter;
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions};
use bitcoin::{Amount, OutPoint, psbt};
use regtest::Regtest;
use rgb_lib::wallet::RgbWalletOpsOffline;
use rgb_lib::wallet::rust_only::{AssetColoringInfo, ColoringInfo};
use rgb_lib::{Assignment, ContractId, FileContent};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

const SELLS: u64 = 400;
const PRICE_SAT: u64 = 30_000;
const CARRIER_SAT: u64 = 1_000;
/// value of the opret output: stock arkd wants it in [vtxoMinAmount, dust)
const OPRET_SAT: u64 = 1;
const BL: u64 = 8101;

pub async fn run() -> anyhow::Result<()> {
    let run_id = format!("run-{}-stock-lane", now());
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

    step("1. stock arkd? Alice (asset) and Bob (bitcoin) board");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    rec("arkd_version", info.version.clone());
    rec("vtxo_min_amount", format!("{:?}", info.vtxo_min_amount));
    rec("dust", info.dust.to_sat().to_string());
    if info.version.contains("rgb") {
        println!("WARNING: arkd {} is the RGB-patched build; this scenario is meant for stock arkd (REGTEST_PROFILE=stock)", info.version);
    }
    board("alice", &alice, "0.001").await?;
    board("bob", &bob, "0.001").await?;
    let first = |l: ark_client::AnnotatedVtxoList| l.spendable_offchain().next().map(|v| v.vtxo().outpoint);
    let av = first(alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?).ok_or_else(|| anyhow!("alice has no VTXO"))?;
    let bv = first(bob.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?).ok_or_else(|| anyhow!("bob has no VTXO"))?;
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;

    step("2. jump-in onto the predicted checkpoint of Alice's next spend");
    let s_in = alice.resolve_selected_send_inputs(&[av]).await?;
    let b_in = bob.resolve_selected_send_inputs(&[bv]).await?;
    let (s_sat, b_sat) = (s_in[0].amount().to_sat(), b_in[0].amount().to_sat());
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (alice_addr, _) = alice.get_offchain_address().await?;
    let (bob_change, _) = bob.get_offchain_address().await?;
    // exact outputs (no change output)
    let receivers = vec![
        SendReceiver::bitcoin(bob_addr, Amount::from_sat(CARRIER_SAT)),
        SendReceiver::bitcoin(alice_addr.clone(), Amount::from_sat(s_sat - CARRIER_SAT + PRICE_SAT)),
        SendReceiver::bitcoin(bob_change, Amount::from_sat(b_sat - PRICE_SAT)),
    ];
    let OffchainTransactions { mut ark_tx, checkpoint_txs } = build_offchain_transactions(&receivers, &alice_addr, &[s_in[0].clone(), b_in[0].clone()], &info)?;
    // a checkpoint depends only on its VTXO: the one of the joint tx is the one predicted now
    let cp_s = checkpoint_txs[0].unsigned_tx.compute_txid();
    rec("predicted_checkpoint", cp_s.to_string());
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, OutPoint { txid: cp_s, vout: 0 }, &state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    rec("asset_id", asset_id.clone());
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;

    step("3. swap: 400 units for 30,000 sat, commitment only in the ark tx (1-sat opret)");
    // [OP_RETURN 1 sat] [Bob carrier + 400] [Alice price + 600 change] [Bob change] [anchor];
    // the opret's sat come out of Alice's output, after the build (no extra change output)
    ark_tx.unsigned_tx.output[1].value -= Amount::from_sat(OPRET_SAT);
    ark_tx.unsigned_tx.output.insert(0, bare_opret(Amount::from_sat(OPRET_SAT)));
    ark_tx.outputs.insert(0, psbt::Output::default());
    let mut colored = ark_tx.clone();
    let coloring = ColoringInfo {
        // rgb-lib shifts output_map vouts by +1 past our first OP_RETURN
        asset_info_map: HashMap::from([(cid, AssetColoringInfo { output_map: HashMap::from([(0, SELLS), (1, ISSUE_AMOUNT - SELLS)]), static_blinding: Some(BL) })]),
        static_blinding: Some(BL),
        nonce: None,
    };
    let transfers = bl(|| alice_rgb.color_psbt_and_consume(&mut colored, coloring))?;
    ark_tx.unsigned_tx.output[0].script_pubkey = colored.unsigned_tx.output[0].script_pubkey.clone();
    let ark_txid = ark_tx.unsigned_tx.compute_txid();
    ensure!(ark_txid == colored.unsigned_tx.compute_txid(), "ark txid changed after the commitment");
    let opret = ark_tx.unsigned_tx.output[0].script_pubkey.clone();
    rec("opret", format!("{} ({} sat)", opret.to_hex_string(), OPRET_SAT));
    let consignment = state.join("swap.rgbc");
    transfers[0].save_file(&consignment).map_err(|e| anyhow!("save consignment: {e}"))?;

    let got = bl(|| bob_rgb.validate_consignment_offchain(bob_online.clone(), consignment.clone(), vec![ark_txid.to_string()], ark_txid.to_string(), 1))?;
    let units: u64 = got.iter().map(|a| if let Assignment::Fungible(n) = a { *n } else { 0 }).sum();
    ensure!(units == SELLS, "consignment gives Bob {units}, agreed {SELLS}");
    println!("[bob] checked before signing: {units} units on ark tx vout 1");
    let seller = Side { name: "alice", ark: &alice, rgb: &alice_rgb, online: &alice_online, vtxo: av };
    let buyer = Side { name: "bob", ark: &bob, rgb: &bob_rgb, online: &bob_online, vtxo: bv };
    let submitted = complete(&seller, &buyer, ark_tx, checkpoint_txs, ark_txid).await;
    rec("stock_arkd_accepts_swap", match &submitted { Ok(_) => "yes".into(), Err(e) => format!("no: {e:#}").chars().take(300).collect() });
    submitted?;
    rec("ark_txid", ark_txid.to_string());

    step("4. Bob validates with arkd paused: consignment + Ark proof + bitcoin chain");
    let pp = proof_path(&consignment);
    build_proof(&alice, &regtest, &[ark_txid], &[], &pp).await?;
    let ops = operators(&info);
    {
        let _down = arkd_down(&mut rec, "bob")?;
        let reg = import(&bob_rgb, &bob_online, &pp, &ops)?;
        rec("bob_proof_registered", format!("{reg:?}"));
        let (_, got) = bl(|| bob_rgb.accept_transfer_consignment_offchain(bob_online.clone(), consignment.clone(), vec![], ark_txid.to_string(), 1, BL))?;
        rec("bob_accepted", format!("{got:?}"));
        let units: u64 = got.iter().map(|a| if let Assignment::Fungible(n) = a { *n } else { 0 }).sum();
        ensure!(units == SELLS, "Bob accepted {units}, expected {SELLS}");
    }
    let alice_vtxo = OutPoint { txid: ark_txid, vout: 2 };
    let alice_sat = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().find(|v| v.vtxo().outpoint == alice_vtxo).map(|v| v.vtxo().amount.to_sat()).unwrap_or(0);
    rec("alice_new_vtxo_sat", alice_sat.to_string());

    step("5a. limit: a second hop needs a commitment in Bob's checkpoint");
    let bob_vtxo = OutPoint { txid: ark_txid, vout: 1 };
    let b2 = bob.resolve_selected_send_inputs(&[bob_vtxo]).await?;
    let (alice_addr2, _) = alice.get_offchain_address().await?;
    let (bob_change2, bob_change2_vtxo) = bob.get_offchain_address().await?;
    let hop = vec![b2[0].clone().with_checkpoint_op_return(Some([7u8; 32]))];
    let OffchainTransactions { ark_tx: hop_tx, checkpoint_txs: hop_cps } =
        build_offchain_transactions(&[SendReceiver::bitcoin(alice_addr2, Amount::from_sat(CARRIER_SAT / 2))], &bob_change2, &hop, &info)?;
    let second = bob.submit_built_offchain_send(hop_tx, hop_cps, bob_change2_vtxo.owner_pk()).await;
    rec("second_hop_with_checkpoint_commitment", match &second { Ok(_) => "ACCEPTED".into(), Err(e) => format!("rejected: {e:?}").chars().take(300).collect() });

    step("5b. limit: does arkd index the opret output as a VTXO?");
    let as_vtxo = crate::long_offline::vtxo_info(OutPoint { txid: ark_txid, vout: 0 }).await;
    rec("opret_indexed_as_vtxo", match &as_vtxo { Some(v) => format!("yes: {} sat, spendable {}, swept {}", v.amount.to_sat(), !v.is_spent, v.is_swept), None => "no".into() });
    let payload: [u8; 32] = opret.as_bytes()[2..34].try_into()?;
    let valid_point = bitcoin::XOnlyPublicKey::from_slice(&payload).is_ok();
    rec("opret_payload_is_a_valid_xonly_key", valid_point.to_string());

    step("5c. limit: does arkd still restart?");
    let restarted = std::process::Command::new("docker").args(["restart", "arkd"]).status()?.success();
    let mut up = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if crate::long_offline::vtxo_info(bob_vtxo).await.is_some() {
            up = true;
            break;
        }
    }
    rec("arkd_restart", format!("docker restart ok {restarted}, serving again within 90 s: {up}"));

    let a = bl(|| alice_rgb.get_asset_balance(asset_id.clone()))?;
    rec("alice_rgb_balance", format!("{a:?}"));
    println!("STOCK LANE RESULT: PASS (swap on stock arkd; limits recorded above)");
    Ok(())
}

//! Scenario `basic`: jump-in + one off-chain hop + unilateral exit + on-chain send, on stock arkd semantics.
//!
//! Alice boards, puts an RGB NIA asset on the *predicted* checkpoint output of her next off-chain
//! send (either issued there directly, or issued on-chain and moved there with a jump-in tx when
//! `onchain_genesis`), sends an Ark tx to Bob carrying the RGB opret commitment, Bob accepts the
//! consignment, exits unilaterally, and after the exit delay sends the asset on-chain to Carol
//! (a plain rgb-lib wallet).
#![allow(unused_imports)]
use crate::common::*;
use anyhow::{Context, anyhow, bail, ensure};
use ark_bdk_wallet::Wallet as ArkBdkWallet;
use ark_client::{Client, InMemorySwapStorage, OfflineClient, OfflineClientConfig, GetVtxosRequestFilter};
use ark_client::wallet::OnchainWallet;
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions};
use bitcoin::bip32::Xpriv;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Address, Amount, Network, OutPoint, Psbt, ScriptBuf, TxOut, Txid, psbt};
use crate::regtest::{self, Regtest};
use rgb_lib::wallet::rust_only::{AssetColoringInfo, ColoringInfo};
use rgb_lib::wallet::{DatabaseType, Online, OnlineOptions, Outpoint, RgbWalletOpsOffline, RgbWalletOpsOnline, SinglesigKeys, Wallet as RgbWallet, WalletData};
use rgb_lib::{AssetSchema, Assignment, BitcoinNetwork, ContractId, FileContent, RgbTransfer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;


pub async fn run(onchain_genesis: bool) -> anyhow::Result<()> {
    let run_id = format!("{}{}", chrono_like_id(), if onchain_genesis { "-onchain-genesis" } else { "" });
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

    // ---------------------------------------------------------------- step 2
    step("2. board Alice and Bob");
    let (alice, _alice_w) = ark_client("alice", regtest.clone(), &state).await?;
    let (bob, bob_w) = ark_client("bob", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    println!("arkd: signer {} dust {} vtxo_min_amount {:?} unilateral_exit_delay {:?}", info.signer_pk, info.dust, info.vtxo_min_amount, info.unilateral_exit_delay);
    board("alice", &alice, ALICE_BOARD_BTC).await?;
    board("bob", &bob, BOB_BOARD_BTC).await?;
    let alice_vtxos = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?;
    let alice_vtxo = alice_vtxos.spendable_offchain().next().ok_or_else(|| anyhow!("alice has no VTXO"))?.vtxo().clone();
    println!("[alice] VTXO {} amount {} expires_at {}", alice_vtxo.outpoint, alice_vtxo.amount, alice_vtxo.expires_at);
    rec("alice_vtxo", alice_vtxo.outpoint.to_string(), &mut summary);

    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;
    let (mut carol_rgb, carol_online) = bl(|| rgb_wallet("carol", &state))?;
    let _ = &mut alice_rgb;

    // ---------------------------------------------------------------- step 3
    step(if onchain_genesis { "3. (on-chain genesis) predict checkpoint txid; issue NIA on-chain; jump-in transfer to cp:0" } else { "3. predict checkpoint txid and issue NIA to cp:0" });
    let inputs = alice.resolve_selected_send_inputs(&[alice_vtxo.outpoint]).await?;
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (alice_change, alice_change_vtxo) = alice.get_offchain_address().await?;
    let receivers = vec![SendReceiver::bitcoin(bob_addr, Amount::from_sat(ALICE_TO_BOB_SAT))];
    let dry = build_offchain_transactions(&receivers, &alice_change, &inputs, &info)?;
    let predicted_cp = dry.checkpoint_txs[0].unsigned_tx.compute_txid();
    rec("predicted_checkpoint_txid", predicted_cp.to_string(), &mut summary);
    let asset_id = if onchain_genesis {
        onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, OutPoint { txid: predicted_cp, vout: 0 }, &state, &mut |k, v| rec(k, v, &mut summary)).await?
    } else {
        bl(|| {
            alice_rgb.issue_asset_nia_to_outpoint(
                "ARKT".into(),
                "Arkade RGB Test".into(),
                0,
                ISSUE_AMOUNT,
                Outpoint { txid: predicted_cp.to_string(), vout: 0 },
            )
        })?
    };
    rec("asset_id", asset_id.clone(), &mut summary);
    let contract_id = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    println!("[alice] list_assets: {:?}", bl(|| alice_rgb.list_assets(vec![]))?.nia);

    // ---------------------------------------------------------------- step 4
    step("4. build, color, submit and finalize the Alice->Bob ark tx");
    let OffchainTransactions { mut ark_tx, checkpoint_txs } =
        build_offchain_transactions(&receivers, &alice_change, &inputs, &info)?;
    let cp_txid = checkpoint_txs[0].unsigned_tx.compute_txid();
    if cp_txid != predicted_cp {
        bail!("checkpoint txid changed between dry run and real build: {cp_txid} vs {predicted_cp}");
    }
    // outputs: [bob, alice_change, anchor] -> [opret(1 sat), bob, alice_change(-1 sat), anchor]
    println!("ark tx outputs before: {:?}", ark_tx.unsigned_tx.output.iter().map(|o| (o.value.to_sat(), o.script_pubkey.to_hex_string())).collect::<Vec<_>>());
    {
        let change = &mut ark_tx.unsigned_tx.output[1];
        change.value = change.value.checked_sub(Amount::ONE_SAT).ok_or_else(|| anyhow!("no change"))?;
    }
    ark_tx.unsigned_tx.output.insert(0, bare_opret(Amount::ONE_SAT));
    ark_tx.outputs.insert(0, psbt::Output::default());
    let (transfers, ark_txid) = color_and_commit(&alice_rgb, &mut ark_tx, contract_id, 1, ISSUE_AMOUNT, BOB_BLINDING)?;
    println!("ark tx outputs after:  {:?}", ark_tx.unsigned_tx.output.iter().map(|o| (o.value.to_sat(), o.script_pubkey.to_hex_string())).collect::<Vec<_>>());
    rec("ark_txid_local", ark_txid.to_string(), &mut summary);
    let consignment_ab = state.join("alice_to_bob.rgbc");
    transfers[0].save_file(&consignment_ab).map_err(|e| anyhow!("save consignment: {e}"))?;
    rec("consignment_alice_to_bob", consignment_ab.display().to_string(), &mut summary);

    let pending = alice
        .submit_built_offchain_send(ark_tx, checkpoint_txs, alice_change_vtxo.owner_pk())
        .await
        .map_err(|e| anyhow!("submit: {e:?}"))?;
    rec("ark_txid_server", pending.ark_txid.to_string(), &mut summary);
    let server_cp: Vec<Txid> = pending.signed_checkpoint_txs.iter().map(|p| p.unsigned_tx.compute_txid()).collect();
    rec("checkpoint_txid_server", format!("{server_cp:?}"), &mut summary);
    if pending.ark_txid != ark_txid {
        bail!("server ark txid {} != local {ark_txid}", pending.ark_txid);
    }
    if server_cp != vec![predicted_cp] {
        bail!("server checkpoint {server_cp:?} != predicted {predicted_cp}");
    }
    alice.sign_and_finalize_pending_tx(pending).await.map_err(|e| anyhow!("finalize: {e:?}"))?;
    println!("[alice] ark tx finalized; checkpoint txid == prediction: OK");

    // ---------------------------------------------------------------- step 5
    step("5. Bob accepts the consignment");
    let (_, assignments, _) = bl(|| {
        bob_rgb.accept_transfer_consignment(bob_online.clone(), consignment_ab.clone(), ark_txid.to_string(), 1, BOB_BLINDING)
    })?;
    rec("bob_accept_assignments", format!("{assignments:?}"), &mut summary);
    let bob_vtxos = bob.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?;
    for v in bob_vtxos.spendable_offchain() {
        println!("[bob] VTXO {} {} preconfirmed={}", v.vtxo().outpoint, v.vtxo().amount, v.vtxo().is_preconfirmed);
    }

    // ---------------------------------------------------------------- step 6
    step("6. Bob unilateral exit of the RGB VTXO");
    let bob_onchain = bob.get_onchain_address()?;
    regtest::faucet(&bob_onchain, BOB_ONCHAIN_BTC)?;
    let mut funded = false;
    for _ in 0..40 {
        bob_w.sync().await?;
        let b = bob_w.balance()?;
        if b.confirmed > Amount::ZERO {
            println!("[bob] on-chain (anchor-bump) wallet confirmed balance {}", b.confirmed);
            funded = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    if !funded {
        bail!("bob on-chain wallet never saw the faucet funds");
    }
    let trees = bob.build_unilateral_exit_trees().await?;
    let branch = trees
        .into_iter()
        .find(|b| b.last().map(|t| t.compute_txid()) == Some(ark_txid))
        .ok_or_else(|| anyhow!("no exit branch ending in the ark tx"))?;
    println!("[bob] exit branch: {:?}", branch.iter().map(|t| t.compute_txid()).collect::<Vec<_>>());
    rec("exit_branch", format!("{:?}", branch.iter().map(|t| t.compute_txid().to_string()).collect::<Vec<_>>()), &mut summary);
    for _ in 0..(branch.len() + 5) {
        bob_w.sync().await?;
        match bob.broadcast_next_unilateral_exit_node(&branch).await {
            Ok(Some(txid)) => {
                println!("[bob] broadcast exit node {txid}");
                regtest::mine(1)?;
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Ok(None) => break,
            Err(e) => bail!("exit broadcast failed: {e:?}"),
        }
    }
    let confs = regtest.tx_confirmations(&ark_txid).ok_or_else(|| anyhow!("ark tx not on chain"))?;
    rec("ark_tx_onchain_confirmations", confs.to_string(), &mut summary);
    rec("checkpoint_onchain_confirmations", format!("{:?}", regtest.tx_confirmations(&predicted_cp)), &mut summary);
    let upd = bl(|| bob_rgb.update_witnesses(bob_online.clone(), 0, vec![]))?;
    println!("[bob] update_witnesses: {upd:?}");

    // ---------------------------------------------------------------- step 7
    step("7. after exit delay, Bob sends on-chain to Carol");
    let carol_recv = bl(|| carol_rgb.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
    rec("carol_recipient_id", carol_recv.recipient_id.clone(), &mut summary);
    let carol_script = rgb_lib::utils::script_buf_from_recipient_id(carol_recv.recipient_id.clone())?
        .ok_or_else(|| anyhow!("not a witness recipient"))?;
    let carol_addr = Address::from_script(&carol_script, Network::Regtest)?;
    println!("[carol] receive address {carol_addr}");

    let mut bob_transfers: Option<Vec<RgbTransfer>> = None;
    let mut bob_spend = None;
    for attempt in 0..120 {
        let slot = &mut bob_transfers;
        let rgb_ref = &bob_rgb;
        let hook = move |p: &mut Psbt| -> Result<(), ark_core::Error> {
            let (t, _) = color_and_commit(rgb_ref, p, contract_id, 1, ISSUE_AMOUNT, BOB_BLINDING + 1)
                .map_err(|e| ark_core::Error::ad_hoc(format!("rgb coloring failed: {e:#}")))?;
            *slot = Some(t);
            Ok(())
        };
        match bob
            .create_send_on_chain_transaction_with_hook(
                carol_addr.clone(),
                Amount::from_sat(CAROL_SAT),
                vec![bare_opret(Amount::ZERO)],
                Amount::from_sat(BOB_EXIT_SPEND_FEE_SAT),
                &[OutPoint { txid: ark_txid, vout: 1 }],
                hook,
            )
            .await
        {
            Ok((tx, prevouts)) => {
                println!("[bob] spend inputs: {:?}", tx.input.iter().map(|i| i.previous_output).collect::<Vec<_>>());
                let _ = prevouts;
                bob_spend = Some(tx);
                break;
            }
            Err(e) => {
                if attempt % 6 == 0 {
                    println!("[bob] exit output not spendable yet (attempt {attempt}): {e}");
                }
                regtest::mine(1)?;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    let bob_spend = bob_spend.ok_or_else(|| anyhow!("Bob could not spend the exited VTXO"))?;
    ensure!(bob_spend.input.iter().any(|i| i.previous_output == OutPoint { txid: ark_txid, vout: 1 }), "spend does not include ark_txid:1 (Bob's RGB output)");
    let mut bob_spend_txid = None;
    for i in 0..60 {
        match regtest.send_raw(&bob_spend) {
            Ok(t) => {
                bob_spend_txid = Some(t);
                break;
            }
            Err(e) => {
                // the client checks the CSV delay against wall-clock time, bitcoind against MTP
                if i % 6 == 0 {
                    println!("[bob] broadcast attempt {i}: {e}");
                }
                regtest::mine(1)?;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    let bob_spend_txid = bob_spend_txid.ok_or_else(|| anyhow!("could not broadcast Bob->Carol"))?;
    rec("bob_to_carol_txid", bob_spend_txid.to_string(), &mut summary);
    let consignment_bc = state.join("bob_to_carol.rgbc");
    bob_transfers.ok_or_else(|| anyhow!("no transfers"))?[0]
        .save_file(&consignment_bc)
        .map_err(|e| anyhow!("save consignment: {e}"))?;
    rec("consignment_bob_to_carol", consignment_bc.display().to_string(), &mut summary);

    let mut provided = None;
    for i in 0..10 {
        match bl(|| carol_rgb.provide_out_of_band_consignment(carol_online.clone(), consignment_bc.display().to_string(), vec![])) {
            Ok(r) => {
                provided = Some(r);
                break;
            }
            Err(e) => {
                println!("[carol] provide_out_of_band_consignment attempt {i}: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    println!("[carol] provide_out_of_band_consignment: {:?}", provided.ok_or_else(|| anyhow!("carol could not accept"))?);
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let refreshed = bl(|| carol_rgb.refresh(carol_online.clone(), None, vec![], false))?;
    println!("[carol] refresh: {refreshed:?}");
    let assets = bl(|| carol_rgb.list_assets(vec![]))?;
    println!("[carol] list_assets: {:?}", assets.nia);
    let bal = bl(|| carol_rgb.get_asset_balance(asset_id.clone()))?;
    rec("carol_balance", format!("{bal:?}"), &mut summary);
    let pass = assets.nia.as_ref().is_some_and(|v| v.iter().any(|a| a.asset_id == asset_id)) && bal.settled == ISSUE_AMOUNT;
    rec("PASS", pass.to_string(), &mut summary);
    println!("\nBASIC RESULT: {}", if pass { "PASS" } else { "FAIL" });
    if !pass {
        bail!("Carol balance check failed");
    }
    Ok(())
}

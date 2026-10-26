//! Stage 0 RGB-on-Arkade regtest MVP.
//!
//! Alice boards, issues an RGB NIA asset to the *predicted* checkpoint output of her next
//! offchain send, sends an Ark tx to Bob carrying the RGB opret commitment, Bob accepts the
//! consignment, exits unilaterally, and after the exit delay sends the asset on-chain to Carol
//! (a plain rgb-lib wallet).
mod regtest;
mod stage1;
mod stage2;
mod proof;
mod stage3;
mod wtd;
mod offline2w;
mod bench;
mod stage5;
mod stage6;

use anyhow::{Context, anyhow, bail};
use ark_bdk_wallet::Wallet as ArkBdkWallet;
use ark_client::{Client, InMemorySwapStorage, OfflineClient, OfflineClientConfig, GetVtxosRequestFilter};
use ark_client::wallet::OnchainWallet;
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions};
use bitcoin::bip32::Xpriv;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Address, Amount, Network, OutPoint, Psbt, ScriptBuf, TxOut, Txid, psbt};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::{AssetColoringInfo, ColoringInfo};
use rgb_lib::wallet::{DatabaseType, Online, OnlineOptions, Outpoint, RgbWalletOpsOffline, RgbWalletOpsOnline, SinglesigKeys, Wallet as RgbWallet, WalletData};
use rgb_lib::{AssetSchema, Assignment, BitcoinNetwork, ContractId, FileContent, RgbTransfer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

type ArkClient = Client<Regtest, ArkBdkWallet, InMemorySwapStorage>;

const ISSUE_AMOUNT: u64 = 1_000;
const ALICE_BOARD_BTC: &str = "0.001"; // 100_000 sat
const BOB_BOARD_BTC: &str = "0.0005";
const BOB_ONCHAIN_BTC: &str = "0.01"; // for anchor bumping during exit
const ALICE_TO_BOB_SAT: u64 = 20_000;
const BOB_BLINDING: u64 = 777_777;
const CAROL_SAT: u64 = 2_000;
const BOB_EXIT_SPEND_FEE_SAT: u64 = 600;
const ELECTRUM: &str = "tcp://localhost:50001";

fn step(s: &str) {
    println!("\n==================== {s} ====================");
}

/// rgb-lib does blocking I/O (and may spin its own runtime); keep it off the async executor.
fn bl<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

async fn ark_client(name: &str, regtest: Arc<Regtest>, state: &Path) -> anyhow::Result<(ArkClient, Arc<ArkBdkWallet>)> {
    let secp = Secp256k1::new();
    let mut seed = [0u8; 32];
    let mut sk_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut seed);
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut sk_bytes);
    std::fs::write(state.join(format!("{name}-ark-seed.hex")), hex::encode(seed))?;
    std::fs::write(state.join(format!("{name}-onchain-sk.hex")), hex::encode(sk_bytes))?;
    let kp = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&sk_bytes)?);
    let wallet = Arc::new(ArkBdkWallet::new(kp, Network::Regtest, "http://localhost:3000/api")?);
    let xpriv = Xpriv::new_master(Network::Regtest, &seed)?;
    let client = OfflineClient::with_bip32(
        OfflineClientConfig {
            ark_server_url: "http://localhost:7070".to_string(),
            boltz_url: "http://localhost:9069".to_string(),
            ..Default::default()
        },
        xpriv,
        None,
        regtest,
        wallet.clone(),
        Arc::new(InMemorySwapStorage::default()),
    )
    .connect_with_retries(5)
    .await
    .map_err(|e| anyhow!("{name}: connect: {e:?}"))?;
    Ok((client, wallet))
}

/// Reload a user whose state dir already exists (wallet process restarted after being offline).
async fn ark_client_load(name: &str, regtest: Arc<Regtest>, state: &Path) -> anyhow::Result<(ArkClient, Arc<ArkBdkWallet>)> {
    let secp = Secp256k1::new();
    let seed = hex::decode(std::fs::read_to_string(state.join(format!("{name}-ark-seed.hex")))?.trim())?;
    let sk_bytes = hex::decode(std::fs::read_to_string(state.join(format!("{name}-onchain-sk.hex")))?.trim())?;
    let kp = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&sk_bytes)?);
    let wallet = Arc::new(ArkBdkWallet::new(kp, Network::Regtest, "http://localhost:3000/api")?);
    let xpriv = Xpriv::new_master(Network::Regtest, &seed)?;
    let client = OfflineClient::with_bip32(
        OfflineClientConfig {
            ark_server_url: "http://localhost:7070".to_string(),
            boltz_url: "http://localhost:9069".to_string(),
            ..Default::default()
        },
        xpriv,
        None,
        regtest,
        wallet.clone(),
        Arc::new(InMemorySwapStorage::default()),
    )
    .connect_with_retries(5)
    .await
    .map_err(|e| anyhow!("{name}: connect: {e:?}"))?;
    let rep = client.restore_contracts(20).await.map_err(|e| anyhow!("{name}: restore: {e:?}"))?;
    println!("[{name}] restored contracts: {rep:?}");
    Ok((client, wallet))
}

fn rgb_wallet_load(name: &str, state: &Path) -> anyhow::Result<(RgbWallet, Online)> {
    let mnemonic = std::fs::read_to_string(state.join(format!("{name}-rgb-mnemonic.txt")))?.trim().to_string();
    let keys = rgb_lib::keys::restore_keys(BitcoinNetwork::Regtest, mnemonic, rgb_lib::keys::WitnessVersion::Taproot)?;
    let data_dir = state.join(format!("{name}-rgb"));
    let mut w = RgbWallet::new(
        WalletData {
            data_dir: data_dir.to_string_lossy().to_string(),
            bitcoin_network: BitcoinNetwork::Regtest,
            database_type: DatabaseType::Sqlite,
            max_allocations_per_utxo: 5,
            supported_schemas: vec![AssetSchema::Nia],
        },
        SinglesigKeys::from_keys(&keys, None),
    )?;
    let online = w.go_online(OnlineOptions {
        indexer_url: ELECTRUM.to_string(),
        skip_consistency_check: true,
        vanilla_sync_lookback: 20,
    })?;
    Ok((w, online))
}

fn rgb_wallet(name: &str, state: &Path) -> anyhow::Result<(RgbWallet, Online)> {
    let keys = rgb_lib::keys::generate_keys(BitcoinNetwork::Regtest, rgb_lib::keys::WitnessVersion::Taproot);
    std::fs::write(state.join(format!("{name}-rgb-mnemonic.txt")), &keys.mnemonic)?;
    let data_dir = state.join(format!("{name}-rgb"));
    std::fs::create_dir_all(&data_dir)?;
    let mut w = RgbWallet::new(
        WalletData {
            data_dir: data_dir.to_string_lossy().to_string(),
            bitcoin_network: BitcoinNetwork::Regtest,
            database_type: DatabaseType::Sqlite,
            max_allocations_per_utxo: 5,
            supported_schemas: vec![AssetSchema::Nia],
        },
        SinglesigKeys::from_keys(&keys, None),
    )?;
    let online = w.go_online(OnlineOptions {
        indexer_url: ELECTRUM.to_string(),
        skip_consistency_check: false,
        vanilla_sync_lookback: 20,
    })?;
    Ok((w, online))
}

async fn board(name: &str, client: &ArkClient, btc: &str) -> anyhow::Result<()> {
    let addr = client.get_boarding_address().await?;
    println!("[{name}] boarding address {addr}");
    regtest::faucet(&addr, btc)?;
    let mut rng = rand::thread_rng();
    for i in 0..20 {
        match client.settle(&mut rng).await {
            Ok(Some(txid)) => {
                println!("[{name}] settled boarding output in commitment tx {txid}");
                return Ok(());
            }
            Ok(None) => println!("[{name}] settle: nothing to settle yet (attempt {i})"),
            Err(e) => println!("[{name}] settle attempt {i} failed: {e:?}"),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    bail!("[{name}] could not settle boarding output")
}

/// Bare OP_RETURN placeholder that rgb-lib's opret commit turns into `OP_RETURN <32B commitment>`.
fn bare_opret(value: Amount) -> TxOut {
    TxOut { value, script_pubkey: ScriptBuf::new_op_return([]) }
}

/// Color `psbt` (whose output 0 is a bare OP_RETURN) on a clone with rgb-lib, consume the fascia
/// as Tentative, and copy *only* the resulting commitment script back into output 0.
fn color_and_commit(
    rgb: &RgbWallet,
    psbt: &mut Psbt,
    contract_id: ContractId,
    actual_vout: u32,
    amount: u64,
    blinding: u64,
) -> anyhow::Result<(Vec<RgbTransfer>, Txid)> {
    let mut colored = psbt.clone();
    // rgb-lib's color_psbt shifts every output_map vout by +1 when it detects a P2TR output
    // (opreturn_first); our OP_RETURN is already at index 0, so pass actual_vout - 1.
    let coloring = ColoringInfo {
        asset_info_map: HashMap::from([(
            contract_id,
            AssetColoringInfo { output_map: HashMap::from([(actual_vout - 1, amount)]), static_blinding: Some(blinding) },
        )]),
        static_blinding: Some(blinding),
        nonce: None,
    };
    let transfers = bl(|| rgb.color_psbt_and_consume(&mut colored, coloring))?;
    let script = colored.unsigned_tx.output[0].script_pubkey.clone();
    if !script.is_op_return() || script.len() != 34 {
        bail!("unexpected commitment script {script}");
    }
    psbt.unsigned_tx.output[0].script_pubkey = script;
    let txid = psbt.unsigned_tx.compute_txid();
    if txid != colored.unsigned_tx.compute_txid() {
        bail!("colored txid mismatch");
    }
    Ok((transfers, txid))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();

    if std::env::args().any(|a| a == "--stage1") {
        return stage1::run().await;
    }
    if std::env::args().any(|a| a == "--stage2-refresh") {
        return stage2::run_refresh().await;
    }
    if std::env::args().any(|a| a == "--stage2-exit") {
        return stage2::run_exit().await;
    }
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "--watchtowerd") {
            return wtd::run_daemon(PathBuf::from(&args[i + 1])).await;
        }
        if let Some(i) = args.iter().position(|a| a == "--stage5-setup") {
            return stage5::setup(PathBuf::from(&args[i + 1])).await;
        }
        if let Some(i) = args.iter().position(|a| a == "--stage6-verify-recovery") {
            return stage6::verify_recovery(PathBuf::from(&args[i + 1])).await;
        }
        if args.iter().any(|a| a == "--stage6-multi") {
            return stage6::run_multi().await;
        }
        if args.iter().any(|a| a == "--stage6-fail") {
            return stage6::run_fail().await;
        }
        if args.iter().any(|a| a == "--stage6-fraud") {
            return stage6::run_fraud().await;
        }
        if let Some(i) = args.iter().position(|a| a == "--stage5-return") {
            return stage5::return_user(PathBuf::from(&args[i + 1])).await;
        }
        if let Some(i) = args.iter().position(|a| a == "--bench") {
            return bench::run(PathBuf::from(&args[i + 1]), &args[i + 2].clone(), args[i + 3].parse()?).await;
        }
        if let Some(i) = args.iter().position(|a| a == "--offline2w-setup") {
            return offline2w::setup(PathBuf::from(&args[i + 1])).await;
        }
        if let Some(i) = args.iter().position(|a| a == "--offline2w-return") {
            return offline2w::return_user(&args[i + 1], PathBuf::from(&args[i + 2])).await;
        }
    }
    if std::env::args().any(|a| a == "--watchtower") {
        return stage3::run_watchtower().await;
    }
    if std::env::args().any(|a| a == "--probe") {
        return stage2::run_probe().await;
    }
    if std::env::args().any(|a| a == "--stage2-neg") {
        return stage2::run_neg().await;
    }
    let onchain_genesis = std::env::args().any(|a| a == "--onchain-genesis");
    let run_id = format!("{}{}", chrono_like_id(), if onchain_genesis { "-onchain-genesis" } else { "" });
    let state = PathBuf::from("/workspace/rgb-arkade/state").join(&run_id);
    std::fs::create_dir_all(&state)?;
    println!("run id {run_id}, state dir {}", state.display());
    let mut summary = serde_json::Map::new();
    let rec = |k: &str, v: String, summary: &mut serde_json::Map<String, serde_json::Value>| {
        println!("  >> {k} = {v}");
        summary.insert(k.to_string(), serde_json::Value::String(v));
        std::fs::write(state.join("summary.json"), serde_json::to_string_pretty(summary).unwrap()).unwrap();
    };

    let regtest = Arc::new(Regtest::new());

    // ---------------------------------------------------------------- M2
    step("M2: board Alice and Bob");
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

    // ---------------------------------------------------------------- M3
    step(if onchain_genesis { "M3 (variant B): predict checkpoint txid; issue NIA on-chain; jump-in transfer to cp:0" } else { "M3: predict checkpoint txid and issue NIA to cp:0" });
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

    // ---------------------------------------------------------------- M4
    step("M4: build, color, submit and finalize the Alice->Bob ark tx");
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

    // ---------------------------------------------------------------- M5
    step("M5: Bob accepts the consignment");
    let (_, assignments, _) = bl(|| {
        bob_rgb.accept_transfer_consignment(bob_online.clone(), consignment_ab.clone(), ark_txid.to_string(), 1, BOB_BLINDING)
    })?;
    rec("bob_accept_assignments", format!("{assignments:?}"), &mut summary);
    let bob_vtxos = bob.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?;
    for v in bob_vtxos.spendable_offchain() {
        println!("[bob] VTXO {} {} preconfirmed={}", v.vtxo().outpoint, v.vtxo().amount, v.vtxo().is_preconfirmed);
    }

    // ---------------------------------------------------------------- M6
    step("M6: Bob unilateral exit of the RGB VTXO");
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

    // ---------------------------------------------------------------- M7
    step("M7: after exit delay, Bob sends on-chain to Carol");
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
    if !bob_spend.input.iter().any(|i| i.previous_output == OutPoint { txid: ark_txid, vout: 1 }) {
        println!("WARNING: spend does not include ark_txid:2 (Bob RGB output)?");
    }
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
    println!("\nRESULT: {}", if pass { "PASS" } else { "FAIL" });
    if !pass {
        bail!("Carol balance check failed");
    }
    Ok(())
}

/// Variant B ("on-chain genesis + jump-in"): issue the NIA with stock rgb-lib on a mined wallet
/// UTXO, then move it with a user-built, mined on-chain transfer (opret) to the explicit seal
/// `target` (Stage 0: the predicted checkpoint output `cp:0` of Alice's next offchain spend;
/// Stage 1: Alice's VTXO outpoint itself).
async fn onchain_genesis_jump_in(
    regtest: &Regtest,
    alice_rgb: &mut RgbWallet,
    online: &Online,
    target: OutPoint,
    _state: &Path,
    rec: &mut dyn FnMut(&str, String),
) -> anyhow::Result<String> {
    let addr = bl(|| alice_rgb.get_address())?;
    regtest::faucet(&Address::from_str(&addr)?.assume_checked(), "0.001")?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let n = bl(|| alice_rgb.create_utxos(online.clone(), false, Some(1), Some(5_000), 2, false))?;
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("[alice] created {n} colorable UTXO(s)");
    let asset = bl(|| alice_rgb.issue_asset_nia("ARKT".into(), "Arkade RGB Test".into(), 0, vec![ISSUE_AMOUNT]))?;
    let asset_id = asset.asset_id.clone();
    let unspents = bl(|| alice_rgb.list_unspents(Some(online.clone()), false, false))?;
    let colored = unspents
        .iter()
        .find(|u| u.rgb_allocations.iter().any(|a| a.asset_id.as_deref() == Some(asset_id.as_str())))
        .ok_or_else(|| anyhow!("issued asset allocation not found"))?
        .utxo
        .clone();
    rec("genesis_utxo", colored.outpoint.to_string());
    let genesis_confs = regtest.tx_confirmations(&Txid::from_str(&colored.outpoint.txid)?);
    rec("genesis_utxo_confirmations", format!("{genesis_confs:?}"));

    // T1: [colored UTXO] -> [OP_RETURN(opret host), BTC change]; asset -> explicit seal cp:0
    let change_addr = Address::from_str(&bl(|| alice_rgb.get_address())?)?.assume_checked();
    let fee = 500u64;
    let unsigned = bitcoin::Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: OutPoint { txid: Txid::from_str(&colored.outpoint.txid)?, vout: colored.outpoint.vout },
            script_sig: ScriptBuf::new(),
            sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: bitcoin::Witness::new(),
        }],
        output: vec![
            bare_opret(Amount::ZERO),
            TxOut { value: Amount::from_sat(colored.btc_amount - fee), script_pubkey: change_addr.script_pubkey() },
        ],
    };
    let mut psbt = Psbt::from_unsigned_tx(unsigned)?;
    psbt.inputs[0] = bl(|| alice_rgb.psbt_input_for_utxo(colored.outpoint.clone()))?;
    let contract_id = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    let transfers = bl(|| {
        alice_rgb.color_psbt_and_consume_to_outpoint(
            &mut psbt,
            contract_id,
            Outpoint { txid: target.txid.to_string(), vout: target.vout },
            424_242,
        )
    })?;
    let _ = transfers;
    let signed = bl(|| alice_rgb.sign_psbt(psbt.to_string(), None))?;
    let signed = Psbt::from_str(&signed)?;
    let tx = signed.extract_tx().map_err(|e| anyhow!("extract T1: {e}"))?;
    let t1 = regtest.send_raw(&tx).context("broadcast T1")?;
    rec("jump_in_tx_T1", t1.to_string());
    rec("jump_in_tx_T1_opret", tx.output[0].script_pubkey.to_hex_string());
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let upd = bl(|| alice_rgb.update_witnesses(online.clone(), 0, vec![]))?;
    println!("[alice] update_witnesses after T1: {upd:?}");
    rec("jump_in_tx_T1_confirmations", format!("{:?}", regtest.tx_confirmations(&t1)));
    Ok(asset_id)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn chrono_like_id() -> String {
    format!("run-{}", now())
}

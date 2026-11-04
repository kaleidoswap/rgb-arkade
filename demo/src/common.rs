//! Shared helpers: Ark client / rgb-lib wallet setup, boarding, PSBT coloring, on-chain genesis + jump-in.
use anyhow::{Context, anyhow, bail};
use ark_bdk_wallet::Wallet as ArkBdkWallet;
use ark_client::{Client, InMemorySwapStorage, OfflineClient, OfflineClientConfig};
use bitcoin::bip32::Xpriv;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Address, Amount, Network, OutPoint, Psbt, ScriptBuf, TxOut, Txid};
use crate::regtest::{self, Regtest};
use rgb_lib::wallet::rust_only::{AssetColoringInfo, ColoringInfo};
use rgb_lib::wallet::{DatabaseType, Online, OnlineOptions, Outpoint, RgbWalletOpsOffline, SinglesigKeys, Wallet as RgbWallet, WalletData};
use rgb_lib::{AssetSchema, BitcoinNetwork, ContractId, RgbTransfer};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

pub(crate) type ArkClient = Client<Regtest, ArkBdkWallet, InMemorySwapStorage>;

pub(crate) const ISSUE_AMOUNT: u64 = 1_000;
pub(crate) const ALICE_BOARD_BTC: &str = "0.001"; // 100_000 sat
pub(crate) const BOB_BOARD_BTC: &str = "0.0005";
pub(crate) const BOB_ONCHAIN_BTC: &str = "0.01"; // for anchor bumping during exit
pub(crate) const ALICE_TO_BOB_SAT: u64 = 20_000;
pub(crate) const BOB_BLINDING: u64 = 777_777;
pub(crate) const CAROL_SAT: u64 = 2_000;
pub(crate) const BOB_EXIT_SPEND_FEE_SAT: u64 = 600;

pub(crate) fn step(s: &str) {
    println!("\n==================== {s} ====================");
}

/// rgb-lib does blocking I/O (and may spin its own runtime); keep it off the async executor.
pub(crate) fn bl<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

pub(crate) async fn ark_client(name: &str, regtest: Arc<Regtest>, state: &Path) -> anyhow::Result<(ArkClient, Arc<ArkBdkWallet>)> {
    ark_client_at(name, regtest, state, &crate::paths::arkd_url()).await
}

/// [`ark_client`] against an explicit arkd endpoint.
pub(crate) async fn ark_client_at(name: &str, regtest: Arc<Regtest>, state: &Path, url: &str) -> anyhow::Result<(ArkClient, Arc<ArkBdkWallet>)> {
    ark_client_keyed(name, regtest, state, url, None).await
}

/// [`ark_client_at`]; with `key`, the client signs with that one static key (a delegate D) instead
/// of an HD wallet.
pub(crate) async fn ark_client_keyed(name: &str, regtest: Arc<Regtest>, state: &Path, url: &str, key: Option<Keypair>) -> anyhow::Result<(ArkClient, Arc<ArkBdkWallet>)> {
    let secp = Secp256k1::new();
    let mut seed = [0u8; 32];
    let mut sk_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut seed);
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut sk_bytes);
    std::fs::write(state.join(format!("{name}-ark-seed.hex")), hex::encode(seed))?;
    std::fs::write(state.join(format!("{name}-onchain-sk.hex")), hex::encode(sk_bytes))?;
    let kp = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&sk_bytes)?);
    let wallet = Arc::new(ArkBdkWallet::new(kp, Network::Regtest, &crate::paths::esplora_url())?);
    let xpriv = Xpriv::new_master(Network::Regtest, &seed)?;
    let config = OfflineClientConfig { ark_server_url: url.to_string(), boltz_url: "http://localhost:9069".to_string(), ..Default::default() };
    let offline = match key {
        Some(k) => OfflineClient::with_keypair(config, k, regtest, wallet.clone(), Arc::new(InMemorySwapStorage::default())),
        None => OfflineClient::with_bip32(config, xpriv, None, regtest, wallet.clone(), Arc::new(InMemorySwapStorage::default())),
    };
    let client = offline
    .connect_with_retries(5)
    .await
    .map_err(|e| anyhow!("{name}: connect: {e:?}"))?;
    Ok((client, wallet))
}

/// Reload a user whose state dir already exists (wallet process restarted after being offline).
pub(crate) async fn ark_client_load(name: &str, regtest: Arc<Regtest>, state: &Path) -> anyhow::Result<(ArkClient, Arc<ArkBdkWallet>)> {
    let secp = Secp256k1::new();
    let seed = hex::decode(std::fs::read_to_string(state.join(format!("{name}-ark-seed.hex")))?.trim())?;
    let sk_bytes = hex::decode(std::fs::read_to_string(state.join(format!("{name}-onchain-sk.hex")))?.trim())?;
    let kp = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&sk_bytes)?);
    let wallet = Arc::new(ArkBdkWallet::new(kp, Network::Regtest, &crate::paths::esplora_url())?);
    let xpriv = Xpriv::new_master(Network::Regtest, &seed)?;
    let client = OfflineClient::with_bip32(
        OfflineClientConfig {
            ark_server_url: crate::paths::arkd_url(),
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

pub(crate) fn rgb_wallet_load(name: &str, state: &Path) -> anyhow::Result<(RgbWallet, Online)> {
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
        indexer_url: crate::paths::electrum_url(),
        skip_consistency_check: true,
        vanilla_sync_lookback: 20,
    })?;
    Ok((w, online))
}

pub(crate) fn rgb_wallet(name: &str, state: &Path) -> anyhow::Result<(RgbWallet, Online)> {
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
        indexer_url: crate::paths::electrum_url(),
        skip_consistency_check: false,
        vanilla_sync_lookback: 20,
    })?;
    Ok((w, online))
}

pub(crate) async fn board(name: &str, client: &ArkClient, btc: &str) -> anyhow::Result<()> {
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
pub(crate) fn bare_opret(value: Amount) -> TxOut {
    TxOut { value, script_pubkey: ScriptBuf::new_op_return([]) }
}

/// Color `psbt` (whose output 0 is a bare OP_RETURN) on a clone with rgb-lib, consume the fascia
/// as Tentative, and copy *only* the resulting commitment script back into output 0.
pub(crate) fn color_and_commit(
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

/// Variant B ("on-chain genesis + jump-in"): issue the NIA with stock rgb-lib on a mined wallet
/// UTXO, then move it with a user-built, mined on-chain transfer (opret) to the explicit seal
/// `target` (scenario `basic`: the predicted checkpoint output `cp:0` of Alice's next offchain
/// spend; other scenarios: Alice's VTXO outpoint itself).
pub(crate) async fn onchain_genesis_jump_in(
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

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

pub(crate) fn chrono_like_id() -> String {
    format!("run-{}", now())
}


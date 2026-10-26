//! RGB commitment in the *forfeit* tx (requires the patched `arkd:rgb`).
//!
//! The forfeit tx F of a colored VTXO V is the only tx that spends V when V is refreshed into a
//! new batch or collaboratively exited, so F carries `OP_RETURN <32B>` (index 0, 0 sat) closing
//! V's seal. F is only mined if the operator has to punish a double spend, so every downstream
//! validator first *attests* F against the Ark layer (F in `GetForfeitTxs(C)`, its connector input
//! in `GetConnectors(C)`, commitment tx C mined) and registers it in rgb-lib as an Ark-attested
//! witness (resolved as Tentative unless mined).
//!
//! `scenario refresh`: jump-in to V -> refresh V -> V' (T_f in F) -> hop V' -> Bob (T1/T2) ->
//!     Bob unilateral exit -> Bob sends on-chain to Carol. PASS = Carol settled.
//! `scenario collab-exit`: jump-in to V -> collaborative exit of V to Alice's on-chain address O = C:vout
//!     (T_f in F) -> Alice spends O on-chain to Dave. PASS = Dave settled.
//! `scenario forfeit-negatives`: negative tests (malformed checkpoint / forfeit OP_RETURNs rejected by arkd,
//!     attestation of a commitment-less forfeit fails).
use crate::offchain_transfer::{HopResult, arkd_virtual_txs_known, outs, rgb_hop, wait_vtxo};
use crate::{
    ArkClient, ISSUE_AMOUNT, ark_client, bare_opret, bl, board, color_and_commit, now,
    onchain_genesis_jump_in, regtest, rgb_wallet, step,
};
use anyhow::{anyhow, bail, ensure};
use ark_bdk_wallet::Wallet as ArkBdkWallet;
use ark_client::{ForfeitHookContext, GetVtxosRequestFilter};
use ark_client::wallet::OnchainWallet;
use ark_core::ArkAddress;
use ark_core::send::{SendReceiver, build_offchain_transactions};
use bitcoin::{Address, Amount, Network, OutPoint, Psbt, ScriptBuf, Transaction, TxOut, Txid, psbt};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::{Online, Outpoint, RgbWalletOpsOffline, RgbWalletOpsOnline, Wallet as RgbWallet};
use rgb_lib::{Assignment, ContractId, FileContent, RgbTransfer};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

const BL_FORFEIT: u64 = 111;
const BL_CP_A: u64 = 101;
const BL_BOB: u64 = 202;
const BL_CAROL: u64 = 404;
const BL_DAVE: u64 = 606;
const ALICE_TO_BOB_SAT: u64 = 20_000;
const EXIT_SAT: u64 = 20_000;
const RECEIVER_SAT: u64 = 2_000;
const ONCHAIN_BTC: &str = "0.01";
const EXIT_SPEND_FEE_SAT: u64 = 600;

pub(crate) type Rec<'a> = dyn FnMut(&str, String) + 'a;

/// What the forfeit hook produced for the colored VTXO.
pub(crate) struct ForfeitColoring {
    pub(crate) forfeit_tx: Transaction,
    pub(crate) target: OutPoint,
    pub(crate) transfer: RgbTransfer,
}

#[derive(Clone)]
pub(crate) enum BatchOp {
    /// refresh into a new VTXO paying `ArkAddress` (seal = matching VTXO-tree leaf output)
    Refresh(ArkAddress),
    /// collaborative exit to an on-chain address (seal = matching commitment-tx output)
    Exit(Address, Amount),
}

/// Find the new seal (V' or C:vout) in the batch being finalized.
fn batch_target(op: &BatchOp, ctx: &ForfeitHookContext<'_>) -> anyhow::Result<OutPoint> {
    match op {
        BatchOp::Refresh(a) => {
            let spk = a.to_p2tr_script_pubkey();
            ctx.vtxo_tree_leaves
                .iter()
                .find_map(|tx| {
                    tx.output.iter().position(|o| o.script_pubkey == spk).map(|v| OutPoint { txid: tx.compute_txid(), vout: v as u32 })
                })
                .ok_or_else(|| anyhow!("refresh output not found in the VTXO tree leaves"))
        }
        BatchOp::Exit(a, amt) => {
            let spk = a.script_pubkey();
            let c = ctx.commitment_tx;
            c.output
                .iter()
                .position(|o| o.script_pubkey == spk && o.value == *amt)
                .map(|v| OutPoint { txid: c.compute_txid(), vout: v as u32 })
                .ok_or_else(|| anyhow!("exit output not found in the commitment tx"))
        }
    }
}

/// Insert a bare OP_RETURN at index 0 of the forfeit PSBT, color a clone (asset -> explicit seal
/// `target`), and copy only the 32-byte commitment back (no RGB PSBT fields reach arkd).
fn color_forfeit(
    rgb: &RgbWallet,
    contract_id: ContractId,
    amount: u64,
    target: OutPoint,
    p: &mut Psbt,
) -> anyhow::Result<ForfeitColoring> {
    ensure!(!p.unsigned_tx.output.iter().any(|o| o.script_pubkey.is_op_return()), "forfeit already has an OP_RETURN");
    p.unsigned_tx.output.insert(0, bare_opret(Amount::ZERO));
    p.outputs.insert(0, psbt::Output::default());
    let mut colored = p.clone();
    let seal = SealTarget::Explicit(Outpoint { txid: target.txid.to_string(), vout: target.vout });
    let transfer = bl(|| rgb.color_psbt_and_consume_custom(&mut colored, contract_id, vec![(seal, amount, BL_FORFEIT)]))?;
    let c = colored.unsigned_tx.output[0].script_pubkey.clone();
    ensure!(c.is_op_return() && c.len() == 34, "bad forfeit commitment {c}");
    p.unsigned_tx.output[0].script_pubkey = c;
    ensure!(p.unsigned_tx.compute_txid() == colored.unsigned_tx.compute_txid(), "forfeit txid mismatch after commit");
    Ok(ForfeitColoring { forfeit_tx: p.unsigned_tx.clone(), target, transfer })
}

/// Run one batch (refresh or collaborative exit) of `vtxo` with a forfeit hook; retries whole
/// batches. `mode` lets the negative tests tamper with the forfeit instead of coloring it.
pub(crate) async fn hooked_batch(
    label: &str,
    client: &ArkClient,
    vtxo: OutPoint,
    op: BatchOp,
    hook_body: &mut dyn FnMut(&ForfeitHookContext<'_>, &mut Psbt) -> anyhow::Result<()>,
    attempts: usize,
) -> anyhow::Result<Txid> {
    let mut last = None;
    for attempt in 0..attempts {
        let mut hook = |ctx: &ForfeitHookContext<'_>, p: &mut Psbt| -> Result<(), ark_core::Error> {
            if ctx.vtxo != vtxo {
                return Ok(());
            }
            hook_body(ctx, p).map_err(|e| ark_core::Error::ad_hoc(format!("forfeit hook: {e:#}")))
        };
        let mut rng = rand::thread_rng();
        let fut = async {
            match &op {
                BatchOp::Refresh(a) => client.settle_vtxos_with_forfeit_hook(&mut rng, &[vtxo], *a, &mut hook).await,
                BatchOp::Exit(a, amt) => {
                    client.collaborative_redeem_vtxos_with_forfeit_hook(&mut rng, &[vtxo], a.clone(), *amt, &mut hook).await
                }
            }
        };
        match tokio::time::timeout(Duration::from_secs(90), fut).await {
            Ok(Ok(txid)) => {
                println!("[{label}] batch finalized, commitment tx {txid}");
                return Ok(txid);
            }
            Ok(Err(e)) => {
                println!("[{label}] batch attempt {attempt} failed: {e:?}");
                last = Some(format!("{e:?}"));
            }
            Err(_) => {
                println!("[{label}] batch attempt {attempt} timed out");
                last = Some("timeout".into());
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    bail!("[{label}] batch failed: {}", last.unwrap_or_default())
}

/// Colored refresh / collaborative exit of `vtxo` holding `amount` of `contract_id`.
pub(crate) async fn colored_batch(
    label: &str,
    client: &ArkClient,
    rgb: &RgbWallet,
    contract_id: ContractId,
    vtxo: OutPoint,
    amount: u64,
    op: BatchOp,
) -> anyhow::Result<(Txid, ForfeitColoring)> {
    let mut out: Option<ForfeitColoring> = None;
    let op2 = op.clone();
    let commitment = {
        let mut body = |ctx: &ForfeitHookContext<'_>, p: &mut Psbt| -> anyhow::Result<()> {
            let target = batch_target(&op2, ctx)?;
            let fc = color_forfeit(rgb, contract_id, amount, target, p)?;
            println!(
                "[{label}] forfeit {} commits T_f: {} -> {} (opret {})",
                fc.forfeit_tx.compute_txid(), vtxo, fc.target, fc.forfeit_tx.output[0].script_pubkey.to_hex_string()
            );
            out = Some(fc);
            Ok(())
        };
        // single attempt: a failed attempt already consumed a (never-signed) transition locally
        hooked_batch(label, client, vtxo, op, &mut body, 1).await?
    };
    let fc = out.ok_or_else(|| anyhow!("forfeit hook never ran for {vtxo}"))?;
    Ok((commitment, fc))
}

pub(crate) fn mined(regtest: &Regtest, txid: &Txid) -> bool {
    regtest.tx_confirmations(txid).is_some()
}

pub(crate) async fn mine_until(regtest: &Regtest, txid: &Txid) -> anyhow::Result<()> {
    for _ in 0..20 {
        if mined(regtest, txid) {
            return Ok(());
        }
        regtest::mine(1)?;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("{txid} never mined")
}

/// Ark-layer attestation of a forfeit witness, done by each validating party with its own arkd
/// client, followed by registering the tx in its rgb-lib wallet.
async fn attest_forfeit(
    who: &str,
    client: &ArkClient,
    regtest: &Regtest,
    forfeit: &Transaction,
    vtxo: OutPoint,
    commitment: Txid,
) -> anyhow::Result<String> {
    let ftxid = forfeit.compute_txid();
    ensure!(mined(regtest, &commitment), "commitment tx {commitment} is not mined");
    let oprets: Vec<&TxOut> = forfeit.output.iter().filter(|o| o.script_pubkey.is_op_return()).collect();
    ensure!(
        oprets.len() == 1 && forfeit.output[0].script_pubkey.is_op_return() && forfeit.output[0].script_pubkey.len() == 34
            && forfeit.output[0].value == Amount::ZERO,
        "forfeit {ftxid} lacks a single 0-sat OP_RETURN<32B> at index 0"
    );
    ensure!(forfeit.input.iter().any(|i| i.previous_output == vtxo), "forfeit {ftxid} does not spend {vtxo}");
    let nc = client.network_client();
    let fids = nc.get_forfeit_txids(commitment.to_string()).await.map_err(|e| anyhow!("GetForfeitTxs: {e:?}"))?;
    ensure!(fids.contains(&ftxid.to_string()), "forfeit {ftxid} is not among the {} forfeits of batch {commitment}", fids.len());
    let cids = nc.get_connector_txids(commitment.to_string()).await.map_err(|e| anyhow!("GetConnectors: {e:?}"))?;
    ensure!(
        forfeit.input.iter().any(|i| cids.contains(&i.previous_output.txid.to_string())),
        "forfeit {ftxid} has no connector input from batch {commitment}"
    );
    println!("[{who}] attested forfeit {ftxid}: in GetForfeitTxs({commitment}) ({} forfeits), connector in GetConnectors ({} nodes), commitment mined", fids.len(), cids.len());
    Ok(bitcoin::consensus::encode::serialize_hex(forfeit))
}

pub(crate) async fn attest_and_register(
    who: &str,
    client: &ArkClient,
    rgb: &RgbWallet,
    regtest: &Regtest,
    fc: &ForfeitColoring,
    vtxo: OutPoint,
    commitment: Txid,
) -> anyhow::Result<()> {
    let hex = attest_forfeit(who, client, regtest, &fc.forfeit_tx, vtxo, commitment).await?;
    let t = bl(|| rgb.add_ark_attested_witness(hex, commitment.to_string()))?;
    println!("[{who}] registered Ark-attested RGB witness {t}");
    Ok(())
}

pub(crate) async fn fund_onchain(who: &str, client: &ArkClient, w: &ArkBdkWallet) -> anyhow::Result<()> {
    regtest::faucet(&client.get_onchain_address()?, ONCHAIN_BTC)?;
    for _ in 0..40 {
        w.sync().await?;
        if w.balance()?.confirmed > Amount::ZERO {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    bail!("[{who}] on-chain wallet never saw the faucet funds")
}

/// Unilateral exit of the branch ending in `last_txid`; returns the branch txids.
pub(crate) async fn unilateral_exit(who: &str, client: &ArkClient, w: &ArkBdkWallet, last_txid: Txid) -> anyhow::Result<Vec<Txid>> {
    let branch = client
        .build_unilateral_exit_trees()
        .await?
        .into_iter()
        .find(|b| b.last().map(|t| t.compute_txid()) == Some(last_txid))
        .ok_or_else(|| anyhow!("no exit branch ending in {last_txid}"))?;
    for _ in 0..(branch.len() + 8) {
        w.sync().await?;
        match client.broadcast_next_unilateral_exit_node(&branch).await {
            Ok(Some(txid)) => {
                println!("[{who}] broadcast exit node {txid}");
                regtest::mine(1)?;
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Ok(None) => break,
            Err(e) => bail!("[{who}] exit broadcast failed: {e:?}"),
        }
    }
    Ok(branch.iter().map(|t| t.compute_txid()).collect())
}

/// After the exit delay, spend the exited VTXO on-chain (RGB opret at index 0) to `to`.
pub(crate) async fn spend_exited(
    who: &str,
    client: &ArkClient,
    rgb: &RgbWallet,
    regtest: &Regtest,
    exited: OutPoint,
    to: &Address,
    contract_id: ContractId,
    asset_amount: u64,
    blinding: u64,
    cons_path: &Path,
) -> anyhow::Result<Txid> {
    let mut transfers: Option<Vec<RgbTransfer>> = None;
    let mut spend = None;
    for attempt in 0..120 {
        let slot = &mut transfers;
        let hook = move |p: &mut Psbt| -> Result<(), ark_core::Error> {
            let (t, _) = color_and_commit(rgb, p, contract_id, 1, asset_amount, blinding)
                .map_err(|e| ark_core::Error::ad_hoc(format!("rgb coloring failed: {e:#}")))?;
            *slot = Some(t);
            Ok(())
        };
        match client
            .create_send_on_chain_transaction_with_hook(to.clone(), Amount::from_sat(RECEIVER_SAT), vec![bare_opret(Amount::ZERO)], Amount::from_sat(EXIT_SPEND_FEE_SAT), &[exited], hook)
            .await
        {
            Ok((tx, _)) => {
                spend = Some(tx);
                break;
            }
            Err(e) => {
                if attempt % 6 == 0 {
                    println!("[{who}] exit output not spendable yet (attempt {attempt}): {e}");
                }
                regtest::mine(1)?;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    let spend = spend.ok_or_else(|| anyhow!("[{who}] could not spend the exited VTXO"))?;
    ensure!(spend.input.iter().any(|i| i.previous_output == exited), "spend does not include the RGB output");
    let mut txid = None;
    for i in 0..60 {
        match regtest.send_raw(&spend) {
            Ok(t) => {
                txid = Some(t);
                break;
            }
            Err(e) => {
                if i % 6 == 0 {
                    println!("[{who}] broadcast attempt {i}: {e}");
                }
                regtest::mine(1)?;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    let txid = txid.ok_or_else(|| anyhow!("[{who}] could not broadcast"))?;
    transfers.ok_or_else(|| anyhow!("no transfers"))?[0].save_file(cons_path).map_err(|e| anyhow!("save: {e}"))?;
    Ok(txid)
}

/// Receiver side: negative check (stock resolver must reject the consignment), attestation,
/// positive check, then accept + refresh. Returns the settled balance.
#[allow(clippy::too_many_arguments)]
async fn receive_with_attestation(
    who: &str,
    client: &ArkClient,
    rgb: &mut RgbWallet,
    online: &Online,
    regtest: &Regtest,
    fc: &ForfeitColoring,
    vtxo: OutPoint,
    commitment: Txid,
    cons: &Path,
    asset_id: &str,
    rec: &mut Rec<'_>,
) -> anyhow::Result<u64> {
    let r = bl(|| rgb.validate_consignment_ark(online.clone(), cons.to_path_buf(), false));
    println!("[{who}] NEG: stock resolver on the consignment: {r:?}");
    rec(&format!("neg_{who}_stock_resolver_rejects"), r.is_err().to_string());
    ensure!(r.is_err(), "[{who}] stock resolver accepted a history with an unmined forfeit witness");
    let r = bl(|| rgb.validate_consignment_ark(online.clone(), cons.to_path_buf(), true));
    println!("[{who}] NEG: Ark-aware resolver *before* attestation: {r:?}");
    rec(&format!("neg_{who}_unattested_rejects"), r.is_err().to_string());
    ensure!(r.is_err(), "[{who}] unattested forfeit accepted");
    // offline proof instead of asking arkd; arkd stays paused until settled
    let _down = if crate::proof::offline_mode() {
        let operators = crate::proof::operators(&client.server_info().await?);
        let down = crate::proof::arkd_down(rec, who)?;
        let p = crate::proof::proof_path(cons);
        if who == "dave" {
            let neg = crate::proof::negatives(who, rgb, online, &p, &operators, rec)?;
            ensure!(neg, "a mutated proof was accepted");
        }
        let reg = crate::proof::import(rgb, online, &p, &operators)?;
        println!("[{who}] offline proof registered {reg:?}");
        rec(&format!("{who}_proof_registered"), format!("{reg:?}"));
        Some(down)
    } else {
        attest_and_register(who, client, rgb, regtest, fc, vtxo, commitment).await?;
        None
    };
    // the indexer behind the resolver (Fulcrum) can lag a few seconds behind a just-mined spend
    let mut r = bl(|| rgb.validate_consignment_ark(online.clone(), cons.to_path_buf(), true));
    for i in 1..10 {
        if !matches!(r, Err(rgb_lib::Error::Network { .. })) {
            break;
        }
        println!("[{who}] resolver not caught up yet (attempt {i}): {r:?}");
        tokio::time::sleep(Duration::from_secs(5)).await;
        r = bl(|| rgb.validate_consignment_ark(online.clone(), cons.to_path_buf(), true));
    }
    println!("[{who}] Ark-aware resolver after attestation: {r:?}");
    rec(&format!("{who}_attested_validates"), r.is_ok().to_string());
    r?;
    let mut provided = None;
    for i in 0..10 {
        match bl(|| rgb.provide_out_of_band_consignment(online.clone(), cons.display().to_string(), vec![])) {
            Ok(r) => {
                provided = Some(r);
                break;
            }
            Err(e) => {
                println!("[{who}] provide_out_of_band_consignment attempt {i}: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    println!("[{who}] provide_out_of_band_consignment: {:?}", provided.ok_or_else(|| anyhow!("[{who}] could not accept"))?);
    let mut bal = None;
    for i in 0..5 {
        regtest::mine(1)?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let refreshed = bl(|| rgb.refresh(online.clone(), None, vec![], false))?;
        println!("[{who}] refresh #{i}: {refreshed:?}");
        let b = bl(|| rgb.get_asset_balance(asset_id.to_string()))?;
        let done = b.settled > 0;
        bal = Some(b);
        if done {
            break;
        }
    }
    let bal = bal.unwrap();
    rec(&format!("{who}_balance"), format!("{bal:?}"));
    let transfers = bl(|| rgb.list_transfers(rgb_lib::wallet::AssetFilter::Id(asset_id.to_string()), None))?;
    rec(&format!("{who}_transfers"), format!("{:?}", transfers.iter().map(|t| format!("{:?}", t.status)).collect::<Vec<_>>()));
    Ok(bal.settled)
}

pub(crate) fn summary_rec(state: PathBuf) -> impl FnMut(&str, String) {
    let mut summary = serde_json::Map::new();
    move |k: &str, v: String| {
        println!("  >> {k} = {v}");
        summary.insert(k.to_string(), serde_json::Value::String(v));
        std::fs::write(state.join("summary.json"), serde_json::to_string_pretty(&summary).unwrap()).unwrap();
    }
}

pub(crate) fn new_state(tag: &str) -> anyhow::Result<PathBuf> {
    let run_id = format!("run-{}-{tag}", now());
    let state = crate::paths::state().join(&run_id);
    std::fs::create_dir_all(&state)?;
    println!("run id {run_id}, state dir {}", state.display());
    Ok(state)
}

pub(crate) async fn first_vtxo(client: &ArkClient) -> anyhow::Result<OutPoint> {
    Ok(client
        .list_vtxos_filtered(GetVtxosRequestFilter::Spendable)
        .await?
        .spendable_offchain()
        .next()
        .ok_or_else(|| anyhow!("no VTXO"))?
        .vtxo()
        .outpoint)
}

/// Scenario (a): refresh a colored VTXO, then hop + unilateral exit + on-chain send.
pub async fn run_refresh() -> anyhow::Result<()> {
    let state = new_state("refresh")?;
    let mut rec = summary_rec(state.clone());
    let regtest = Arc::new(Regtest::new());

    step("1. clients, Alice boards, on-chain genesis + jump-in to V");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (bob, bob_w) = ark_client("bob", regtest.clone(), &state).await?;
    let (carol, _) = ark_client("carol", regtest.clone(), &state).await?;
    rec("arkd_version", alice.server_info().await?.version.clone());
    board("alice", &alice, "0.001").await?;
    let v = first_vtxo(&alice).await?;
    rec("alice_vtxo_V", v.to_string());
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;
    let (mut carol_rgb, carol_online) = bl(|| rgb_wallet("carol", &state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, v, &state, &mut rec).await?;
    rec("asset_id", asset_id.clone());
    let contract_id = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;

    step("2. refresh V into a new batch; T_f committed in the forfeit tx (V -> V')");
    let (alice_refresh_addr, _) = alice.get_offchain_address().await?;
    let (c1, fc) = colored_batch("refresh", &alice, &alice_rgb, contract_id, v, ISSUE_AMOUNT, BatchOp::Refresh(alice_refresh_addr)).await?;
    let v2 = fc.target;
    fc.transfer.save_file(state.join("refresh_forfeit.rgbc")).map_err(|e| anyhow!("save: {e}"))?;
    rec("refresh_commitment_txid", c1.to_string());
    rec("refresh_forfeit_txid", fc.forfeit_tx.compute_txid().to_string());
    rec("refresh_forfeit_outputs", format!("{:?}", fc.forfeit_tx.output.iter().map(|o| (o.value.to_sat(), o.script_pubkey.to_hex_string())).collect::<Vec<_>>()));
    rec("alice_vtxo_V2", v2.to_string());
    mine_until(&regtest, &c1).await?;
    wait_vtxo(&alice, v2, "alice").await?;
    arkd_virtual_txs_known(&alice, &[v2.txid]).await?;
    attest_and_register("alice", &alice, &alice_rgb, &regtest, &fc, v, c1).await?;
    let upd = bl(|| alice_rgb.update_witnesses(alice_online.clone(), 0, vec![]))?;
    println!("[alice] update_witnesses after refresh: {upd:?}");

    step("3. hop Alice (V') -> Bob (T1 in checkpoint, T2 in ark tx)");
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (alice_change, alice_change_vtxo) = alice.get_offchain_address().await?;
    let ab: HopResult = rgb_hop(
        "A->B", &alice, &alice_rgb, v2,
        vec![SendReceiver::bitcoin(bob_addr, Amount::from_sat(ALICE_TO_BOB_SAT))],
        &alice_change, alice_change_vtxo.owner_pk(), contract_id,
        vec![(1, ISSUE_AMOUNT, BL_BOB)], BL_CP_A, state.join("alice_to_bob.rgbc"),
    ).await?;
    rec("hop_ab_checkpoint_txid", ab.checkpoint_txid.to_string());
    rec("hop_ab_ark_txid", ab.ark_txid.to_string());
    let bob_vtxo = OutPoint { txid: ab.ark_txid, vout: 1 };
    wait_vtxo(&bob, bob_vtxo, "bob").await?;
    let hop_w = vec![ab.checkpoint_txid, ab.ark_txid];
    let offline = crate::proof::offline_mode();
    rec("offline_proof_mode", offline.to_string());
    let p_ab = crate::proof::proof_path(&ab.consignment);
    let fid = fc.forfeit_tx.compute_txid();
    if offline {
        let st = crate::proof::build_proof(&alice, &regtest, &[fid, ab.checkpoint_txid, ab.ark_txid], &[], &p_ab).await?;
        rec("proof_ab", format!("{} B, {} PSBTs, {} witnesses (consignment {} B)", st.bytes, st.n_psbts, st.n_witnesses, crate::proof::file_len(&ab.consignment)));
        rec("proof_ab_txids", format!("{:?}", crate::proof::psbt_txids(&p_ab)?));
    } else {
        arkd_virtual_txs_known(&bob, &hop_w).await?;
    }
    let operators = crate::proof::operators(&bob.server_info().await?);
    let bob_down = if offline { Some(crate::proof::arkd_down(&mut rec, "bob")?) } else { None };
    // Bob: the history contains the unmined forfeit -> must be rejected until attested
    let neg = bl(|| bob_rgb.validate_consignment_ark(bob_online.clone(), ab.consignment.clone(), true));
    println!("[bob] NEG: consignment with unattested forfeit: {neg:?}");
    rec("neg_bob_unattested_rejects", neg.is_err().to_string());
    ensure!(neg.is_err(), "bob accepted an unattested forfeit");
    let offchain: Vec<String> = if offline {
        let ok = crate::proof::negatives("bob", &bob_rgb, &bob_online, &p_ab, &operators, &mut rec)?;
        ensure!(ok, "a mutated proof was accepted");
        let reg = crate::proof::import(&bob_rgb, &bob_online, &p_ab, &operators)?;
        rec("bob_proof_registered", format!("{reg:?}"));
        vec![]
    } else {
        attest_and_register("bob", &bob, &bob_rgb, &regtest, &fc, v, c1).await?;
        hop_w.iter().map(|t| t.to_string()).collect()
    };
    let (_, bob_assign) = bl(|| bob_rgb.accept_transfer_consignment_offchain(
        bob_online.clone(), ab.consignment.clone(), offchain.clone(), ab.ark_txid.to_string(), 1, BL_BOB,
    ))?;
    rec("bob_accept_assignments", format!("{bob_assign:?}"));
    drop(bob_down);

    step("4. Bob unilateral exit");
    fund_onchain("bob", &bob, &bob_w).await?;
    let branch = unilateral_exit("bob", &bob, &bob_w, ab.ark_txid).await?;
    rec("exit_branch", format!("{:?}", branch.iter().map(|t| t.to_string()).collect::<Vec<_>>()));
    for t in [c1, v2.txid, ab.checkpoint_txid, ab.ark_txid] {
        let c = regtest.tx_confirmations(&t);
        rec(&format!("onchain_confs_{t}"), format!("{c:?}"));
        ensure!(c.is_some(), "{t} not on chain after exit");
    }
    let fmined = regtest.tx_confirmations(&fc.forfeit_tx.compute_txid());
    rec("forfeit_onchain_confs", format!("{fmined:?}"));
    let upd = bl(|| bob_rgb.update_witnesses(bob_online.clone(), 0, vec![]))?;
    println!("[bob] update_witnesses: {upd:?}");

    step("5. Bob sends on-chain to Carol; Carol attests F and accepts");
    let carol_recv = bl(|| carol_rgb.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
    let carol_script = rgb_lib::utils::script_buf_from_recipient_id(carol_recv.recipient_id.clone())?
        .ok_or_else(|| anyhow!("not a witness recipient"))?;
    let carol_addr = Address::from_script(&carol_script, Network::Regtest)?;
    let cons = state.join("bob_to_carol.rgbc");
    let bc = spend_exited("bob", &bob, &bob_rgb, &regtest, bob_vtxo, &carol_addr, contract_id, ISSUE_AMOUNT, BL_CAROL, &cons).await?;
    rec("bob_to_carol_txid", bc.to_string());
    regtest::mine(1)?;
    if offline {
        let st = crate::proof::build_proof(&bob, &regtest, &[fid, ab.checkpoint_txid, ab.ark_txid, bc], &[p_ab.clone()], &crate::proof::proof_path(&cons)).await?;
        rec("proof_bc", format!("{} B, {} PSBTs, {} witnesses (consignment {} B)", st.bytes, st.n_psbts, st.n_witnesses, crate::proof::file_len(&cons)));
    }
    let settled = receive_with_attestation("carol", &carol, &mut carol_rgb, &carol_online, &regtest, &fc, v, c1, &cons, &asset_id, &mut rec).await?;
    let pass = settled == ISSUE_AMOUNT;
    rec("PASS", pass.to_string());
    println!("\nREFRESH RESULT: {}", if pass { "PASS" } else { "FAIL" });
    ensure!(pass, "Carol balance check failed");
    Ok(())
}

/// Scenario (b): collaborative exit of a colored VTXO to an on-chain address, then on-chain send.
pub async fn run_exit() -> anyhow::Result<()> {
    let state = new_state("collab-exit")?;
    let mut rec = summary_rec(state.clone());
    let regtest = Arc::new(Regtest::new());

    step("1. clients, Alice boards, on-chain genesis + jump-in to V");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (dave, _) = ark_client("dave", regtest.clone(), &state).await?;
    rec("arkd_version", alice.server_info().await?.version.clone());
    board("alice", &alice, "0.001").await?;
    let v = first_vtxo(&alice).await?;
    rec("alice_vtxo_V", v.to_string());
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (mut dave_rgb, dave_online) = bl(|| rgb_wallet("dave", &state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, v, &state, &mut rec).await?;
    rec("asset_id", asset_id.clone());
    let contract_id = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;

    step("2. collaborative exit of V to Alice's rgb-lib address; T_f in the forfeit (V -> C:vout)");
    let exit_addr = Address::from_str(&bl(|| alice_rgb.get_address())?)?.assume_checked();
    let (c, fc) = colored_batch("exit", &alice, &alice_rgb, contract_id, v, ISSUE_AMOUNT, BatchOp::Exit(exit_addr.clone(), Amount::from_sat(EXIT_SAT))).await?;
    let o = fc.target;
    fc.transfer.save_file(state.join("exit_forfeit.rgbc")).map_err(|e| anyhow!("save: {e}"))?;
    ensure!(o.txid == c, "exit seal {o} not in commitment {c}");
    rec("exit_commitment_txid", c.to_string());
    rec("exit_forfeit_txid", fc.forfeit_tx.compute_txid().to_string());
    rec("exit_forfeit_outputs", format!("{:?}", fc.forfeit_tx.output.iter().map(|o| (o.value.to_sat(), o.script_pubkey.to_hex_string())).collect::<Vec<_>>()));
    rec("exit_output_O", o.to_string());
    let offline = crate::proof::offline_mode();
    rec("offline_proof_mode", offline.to_string());
    let fid = fc.forfeit_tx.compute_txid();
    if offline {
        // NEG: proof built while the commitment tx C is still unmined (C itself left out)
        let early = state.join("exit_forfeit.rgbc.early.arkproof.json");
        crate::proof::build_proof(&alice, &regtest, &[fid], &[], &early).await?;
        let mut f: crate::proof::ProofFile = serde_json::from_str(&std::fs::read_to_string(&early)?)?;
        f.psbts.retain(|b| Psbt::from_str(b).map(|p| p.unsigned_tx.compute_txid() != c).unwrap_or(true));
        std::fs::write(&early, serde_json::to_string(&f)?)?;
        let ops = crate::proof::operators(&dave.server_info().await?);
        let r = crate::proof::import(&dave_rgb, &dave_online, &early, &ops);
        println!("[dave] NEG proof unmined_commitment (C={c} confs {:?}): {:?}", regtest.tx_confirmations(&c), r.as_ref().map_err(|e| e.to_string()));
        rec("neg_proof_dave_unmined_commitment", match &r { Ok(v) => format!("ACCEPTED {v:?}"), Err(e) => format!("rejected: {e}") });
        ensure!(r.is_err(), "proof with unmined commitment accepted");
    }
    mine_until(&regtest, &c).await?;
    attest_and_register("alice", &alice, &alice_rgb, &regtest, &fc, v, c).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let upd = bl(|| alice_rgb.update_witnesses(alice_online.clone(), 0, vec![]))?;
    println!("[alice] update_witnesses after exit: {upd:?}");

    step("3. Alice spends O on-chain to Dave (opret at index 0)");
    let dave_recv = bl(|| dave_rgb.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
    let dave_script = rgb_lib::utils::script_buf_from_recipient_id(dave_recv.recipient_id.clone())?
        .ok_or_else(|| anyhow!("not a witness recipient"))?;
    rec("dave_recipient_id", dave_recv.recipient_id.clone());
    let mut input = None;
    for i in 0..10 {
        let _ = bl(|| alice_rgb.list_unspents(Some(alice_online.clone()), false, false));
        match bl(|| alice_rgb.psbt_input_for_utxo(Outpoint { txid: o.txid.to_string(), vout: o.vout })) {
            Ok(inp) => {
                input = Some(inp);
                break;
            }
            Err(e) => {
                println!("[alice] O not in BDK yet (attempt {i}): {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    let input = input.ok_or_else(|| anyhow!("alice's BDK wallet never saw O"))?;
    let change = Address::from_str(&bl(|| alice_rgb.get_address())?)?.assume_checked();
    let fee = 600u64;
    let unsigned = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn { previous_output: o, sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME, ..Default::default() }],
        output: vec![
            bare_opret(Amount::ZERO),
            TxOut { value: Amount::from_sat(RECEIVER_SAT), script_pubkey: dave_script.clone() },
            TxOut { value: Amount::from_sat(EXIT_SAT - RECEIVER_SAT - fee), script_pubkey: change.script_pubkey() },
        ],
    };
    let mut psbt = Psbt::from_unsigned_tx(unsigned)?;
    psbt.inputs[0] = input;
    let t = bl(|| alice_rgb.color_psbt_and_consume_custom(&mut psbt, contract_id, vec![(SealTarget::WitnessVout(1), ISSUE_AMOUNT, BL_DAVE)]))?;
    let cons = state.join("alice_to_dave.rgbc");
    t.save_file(&cons).map_err(|e| anyhow!("save: {e}"))?;
    let signed = Psbt::from_str(&bl(|| alice_rgb.sign_psbt(psbt.to_string(), None))?)?;
    let tx = signed.extract_tx().map_err(|e| anyhow!("extract: {e}"))?;
    println!("[alice] O spend outputs {:?}", outs(&Psbt::from_unsigned_tx({ let mut u = tx.clone(); u.input.iter_mut().for_each(|i| i.witness = Default::default()); u })?));
    let ad = regtest.send_raw(&tx)?;
    rec("alice_to_dave_txid", ad.to_string());
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let fmined = regtest.tx_confirmations(&fc.forfeit_tx.compute_txid());
    rec("forfeit_onchain_confs", format!("{fmined:?}"));

    if offline {
        let st = crate::proof::build_proof(&alice, &regtest, &[fid, ad], &[], &crate::proof::proof_path(&cons)).await?;
        rec("proof_ad", format!("{} B, {} PSBTs, {} witnesses (consignment {} B)", st.bytes, st.n_psbts, st.n_witnesses, crate::proof::file_len(&cons)));
        rec("proof_ad_txids", format!("{:?}", crate::proof::psbt_txids(&crate::proof::proof_path(&cons))?));
    }
    step("4. Dave attests F and accepts");
    let settled = receive_with_attestation("dave", &dave, &mut dave_rgb, &dave_online, &regtest, &fc, v, c, &cons, &asset_id, &mut rec).await?;
    let pass = settled == ISSUE_AMOUNT;
    rec("PASS", pass.to_string());
    println!("\nCOLLABORATIVE EXIT RESULT: {}", if pass { "PASS" } else { "FAIL" });
    ensure!(pass, "Dave balance check failed");
    Ok(())
}

/// Negative tests on the Ark side (uncolored VTXOs suffice: arkd only checks the tx shape).
pub async fn run_neg() -> anyhow::Result<()> {
    let state = new_state("forfeit-negatives")?;
    let mut rec = summary_rec(state.clone());
    let regtest = Arc::new(Regtest::new());
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    rec("arkd_version", alice.server_info().await?.version.clone());
    board("alice", &alice, "0.001").await?;
    let mut v = first_vtxo(&alice).await?;
    rec("alice_vtxo", v.to_string());
    let info = alice.server_info().await?;
    let mut pass = true;

    // forfeit tampering cases: (name, mutation)
    type Mutate = fn(&mut Psbt);
    let cases: Vec<(&str, Mutate)> = vec![
        ("forfeit_opret_31B", |p| {
            p.unsigned_tx.output.insert(0, TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return([7u8; 31]) });
            p.outputs.insert(0, psbt::Output::default());
        }),
        ("forfeit_two_oprets", |p| {
            for _ in 0..2 {
                p.unsigned_tx.output.insert(0, TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return([7u8; 32]) });
                p.outputs.insert(0, psbt::Output::default());
            }
        }),
        ("forfeit_opret_not_first", |p| {
            p.unsigned_tx.output.insert(1, TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return([7u8; 32]) });
            p.outputs.insert(1, psbt::Output::default());
        }),
        ("forfeit_opret_nonzero_value", |p| {
            p.unsigned_tx.output.insert(0, TxOut { value: Amount::from_sat(1), script_pubkey: ScriptBuf::new_op_return([7u8; 32]) });
            p.outputs.insert(0, psbt::Output::default());
            p.unsigned_tx.output[1].value -= Amount::from_sat(1);
        }),
    ];
    for (name, mutate) in cases {
        step(&format!("NEG {name}: arkd must reject the batch"));
        let (addr, _) = alice.get_offchain_address().await?;
        let mut body = |_: &ForfeitHookContext<'_>, p: &mut Psbt| -> anyhow::Result<()> {
            mutate(p);
            Ok(())
        };
        let r = hooked_batch(name, &alice, v, BatchOp::Refresh(addr), &mut body, 1).await;
        let rejected = r.is_err();
        rec(&format!("neg_{name}_rejected"), rejected.to_string());
        if let Err(e) = &r {
            rec(&format!("neg_{name}_error"), format!("{e:#}").chars().take(400).collect());
        }
        if !rejected {
            pass = false;
            mine_until(&regtest, r.as_ref().unwrap()).await?;
            v = first_vtxo(&alice).await?;
        }
    }

    step("POS control: well-formed 32B forfeit OP_RETURN accepted, attestation succeeds");
    let (addr, _) = alice.get_offchain_address().await?;
    let mut f_ok: Option<Transaction> = None;
    let c = {
        let mut body = |_: &ForfeitHookContext<'_>, p: &mut Psbt| -> anyhow::Result<()> {
            p.unsigned_tx.output.insert(0, TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return([9u8; 32]) });
            p.outputs.insert(0, psbt::Output::default());
            f_ok = Some(p.unsigned_tx.clone());
            Ok(())
        };
        hooked_batch("pos", &alice, v, BatchOp::Refresh(addr), &mut body, 3).await?
    };
    mine_until(&regtest, &c).await?;
    let f_ok = f_ok.ok_or_else(|| anyhow!("hook did not run"))?;
    let a = attest_forfeit("bob", &bob, &regtest, &f_ok, v, c).await;
    rec("pos_commitment_txid", c.to_string());
    rec("pos_forfeit_txid", f_ok.compute_txid().to_string());
    rec("pos_attestation_ok", a.is_ok().to_string());
    pass &= a.is_ok();
    // wrong VTXO claimed
    let a = attest_forfeit("bob", &bob, &regtest, &f_ok, OutPoint { txid: c, vout: 0 }, c).await;
    rec("neg_attest_wrong_vtxo_rejected", a.is_err().to_string());
    pass &= a.is_err();
    v = {
        let mut found = None;
        for _ in 0..10 {
            if let Ok(o) = first_vtxo(&alice).await {
                found = Some(o);
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        found.ok_or_else(|| anyhow!("no VTXO after pos refresh"))?
    };

    step("NEG forfeit_without_opret: plain refresh; the would-be committed forfeit fails attestation");
    let (addr, _) = alice.get_offchain_address().await?;
    let mut f_claimed: Option<Transaction> = None;
    let c2 = {
        let mut body = |_: &ForfeitHookContext<'_>, p: &mut Psbt| -> anyhow::Result<()> {
            let mut tx = p.unsigned_tx.clone();
            tx.output.insert(0, TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return([5u8; 32]) });
            f_claimed = Some(tx);
            Ok(())
        };
        hooked_batch("noopret", &alice, v, BatchOp::Refresh(addr), &mut body, 3).await?
    };
    mine_until(&regtest, &c2).await?;
    let a = attest_forfeit("bob", &bob, &regtest, f_claimed.as_ref().unwrap(), v, c2).await;
    println!("attestation of commitment-less forfeit: {a:?}");
    rec("neg_forfeit_without_opret_attest_rejected", a.is_err().to_string());
    if let Err(e) = &a {
        rec("neg_forfeit_without_opret_error", format!("{e:#}"));
    }
    pass &= a.is_err();
    // unmined commitment: fake commitment txid
    let a = attest_forfeit("bob", &bob, &regtest, f_claimed.as_ref().unwrap(), v, f_claimed.as_ref().unwrap().compute_txid()).await;
    rec("neg_attest_unmined_commitment_rejected", a.is_err().to_string());
    pass &= a.is_err();
    v = {
        let mut found = None;
        for _ in 0..10 {
            if let Ok(o) = first_vtxo(&alice).await {
                found = Some(o);
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        found.ok_or_else(|| anyhow!("no VTXO after refresh"))?
    };

    // checkpoint tampering (offchain send). Patch the ark tx input to the tampered checkpoint txid
    // so arkd's rejection is about the checkpoint OP_RETURN itself.
    type CpMutate = fn(&mut Psbt);
    let cp_cases: Vec<(&str, CpMutate)> = vec![
        ("checkpoint_opret_31B", |p| p.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([3u8; 31])),
        ("checkpoint_two_oprets", |p| {
            p.unsigned_tx.output.insert(0, TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return([4u8; 32]) });
            p.outputs.insert(0, psbt::Output::default());
        }),
        ("checkpoint_opret_nonzero_value", |p| p.unsigned_tx.output[0].value = Amount::from_sat(1)),
    ];
    for (name, mutate) in cp_cases {
        step(&format!("NEG {name}: arkd must reject the offchain tx"));
        let (bob_addr, _) = bob.get_offchain_address().await?;
        let (chg, chg_vtxo) = alice.get_offchain_address().await?;
        let inputs: Vec<_> = alice.resolve_selected_send_inputs(&[v]).await?
            .into_iter().map(|i| i.with_checkpoint_op_return(Some([1u8; 32]))).collect();
        let built = build_offchain_transactions(&[SendReceiver::bitcoin(bob_addr, Amount::from_sat(5_000))], &chg, &inputs, &info)?;
        let mut ark_tx = built.ark_tx;
        let mut cps = built.checkpoint_txs;
        let old = cps[0].unsigned_tx.compute_txid();
        mutate(&mut cps[0]);
        let new = cps[0].unsigned_tx.compute_txid();
        for i in ark_tx.unsigned_tx.input.iter_mut() {
            if i.previous_output.txid == old {
                i.previous_output.txid = new;
            }
        }
        let r = alice.submit_built_offchain_send(ark_tx, cps, chg_vtxo.owner_pk()).await;
        let rejected = r.is_err();
        rec(&format!("neg_{name}_rejected"), rejected.to_string());
        match r {
            Err(e) => rec(&format!("neg_{name}_error"), format!("{e:?}").chars().take(400).collect()),
            Ok(p) => {
                pass = false;
                println!("UNEXPECTED: arkd accepted {name}: {}", p.ark_txid);
            }
        }
    }
    rec("PASS", pass.to_string());
    println!("\nFORFEIT NEGATIVE TESTS RESULT: {}", if pass { "PASS" } else { "FAIL" });
    ensure!(pass, "negative tests failed");
    Ok(())
}


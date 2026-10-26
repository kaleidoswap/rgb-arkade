//! Batch-level scenarios (rgb-lib Ark registry: tracked VTXOs, Ark balance, rollback, fraud proofs).
//! `scenario multi-user`: one batch with several users, two RGB assets, a
//!     BTC-only VTXO; off-chain send with BTC + asset change before it; every receiver validates
//!     offline (arkd paused) and the rgb-lib Ark balances are checked.
//! `scenario failed-batch`: a colored batch fails mid-way (another participant refuses to sign
//!     its forfeit); the colored forfeit is rolled back in rgb-lib and the refresh is retried.
//! `scenario fraud`: operator + user collude (operator edits its own DB) and co-sign
//!     two conflicting forfeits of the same VTXO in two batches; fraud proof + online detection.
//! `scenario verify-recovery <sweep-recovery dir>`: re-verify a sweep-recovery proof with
//!     the on-chain sweep check, and a not-swept forfeit declared as a recovery (must fail).
use crate::long_offline::vtxo_info;
use crate::proof::{ProofFile, add_vtxos, build_proof, import, mark_recovery, operators, proof_path};
use crate::offchain_transfer::{rgb_hop, wait_vtxo};
use crate::forfeit::{BatchOp, ForfeitColoring, colored_batch, first_vtxo, hooked_batch, mine_until, new_state, summary_rec};
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bl, board, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, bail, ensure};
use ark_core::send::SendReceiver;
use bitcoin::{Amount, OutPoint, Psbt, Txid};
use regtest::Regtest;
use rgb_lib::ark_proof::{ArkFraudProof, ArkProof};
use rgb_lib::wallet::{Online, Wallet as RgbWallet};
use rgb_lib::wallet::RgbWalletOpsOffline;
use rgb_lib::{ContractId, FileContent};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

const SEND_SAT: u64 = 20_000;
const SEND_ASSET: u64 = 300;
const CHANGE_ASSET: u64 = ISSUE_AMOUNT - SEND_ASSET;
const BL_D: u64 = 6101;
const BL_AC: u64 = 6102;
const BL_CP: u64 = 6103;

type Rec<'a> = dyn FnMut(&str, String) + 'a;

/// Track `vtxos` in `rgb` (their branch from arkd -> proof `vtxos` -> import, chain-verified).
async fn track(who: &str, client: &ArkClient, regtest: &Regtest, rgb: &RgbWallet, online: &Online, ops: &str, dir: &Path, vtxos: &[OutPoint]) -> anyhow::Result<()> {
    let p = dir.join(format!("track-{who}-{}.arkproof.json", &vtxos[0].txid.to_string()[..8]));
    add_vtxos(client, regtest, &p, vtxos).await?;
    import(rgb, online, &p, ops)?;
    for v in vtxos {
        own(rgb, *v)?;
    }
    Ok(())
}

/// Declare an own VTXO (change / own refresh output) in rgb-lib.
pub(crate) fn own(rgb: &RgbWallet, v: OutPoint) -> anyhow::Result<()> {
    bl(|| rgb.ark_set_vtxo_owned(v.to_string(), true)).map_err(|e| anyhow!("{e}"))
}

fn ark_bal(rgb: &RgbWallet, online: &Online, cid: &ContractId) -> anyhow::Result<rgb_lib::ark_proof::ArkAssetBalance> {
    bl(|| rgb.get_asset_balance_ark(online.clone(), cid.to_string())).map_err(|e| anyhow!("{e}"))
}

fn bal_str(b: &rgb_lib::ark_proof::ArkAssetBalance) -> String {
    format!("active {} to_recover {} unrolled {} in_flight {} vtxos {:?}", b.active, b.to_recover, b.unrolled, b.in_flight, b.vtxos)
}

/// Owner-side proof for a forfeit `fid` (carrying `carried`), tracking the new VTXO `target`.
async fn forfeit_proof(client: &ArkClient, regtest: &Regtest, fc: &ForfeitColoring, carried: &[PathBuf], cons: &Path) -> anyhow::Result<PathBuf> {
    fc.transfer.save_file(cons).map_err(|e| anyhow!("save: {e}"))?;
    let p = proof_path(cons);
    let mut ws: Vec<Txid> = vec![];
    for c in carried {
        let f: ProofFile = serde_json::from_str(&std::fs::read_to_string(c)?)?;
        ws.extend(f.witnesses.iter().filter_map(|w| Txid::from_str(w).ok()));
    }
    ws.push(fc.forfeit_tx.compute_txid());
    build_proof(client, regtest, &ws, carried, &p).await?;
    add_vtxos(client, regtest, &p, &[fc.target]).await?;
    Ok(p)
}

fn copy_with(src: &Path, name: &str, f: impl Fn(&mut ProofFile)) -> anyhow::Result<PathBuf> {
    let mut pf: ProofFile = serde_json::from_str(&std::fs::read_to_string(src)?)?;
    f(&mut pf);
    let out = src.with_extension(format!("neg-{name}.json"));
    std::fs::write(&out, serde_json::to_string(&pf)?)?;
    Ok(out)
}

async fn colored_user(name: &str, regtest: &Arc<Regtest>, state: &Path, rec: &mut Rec<'_>) -> anyhow::Result<(ArkClient, RgbWallet, Online, OutPoint, ContractId)> {
    let (c, _) = ark_client(name, regtest.clone(), state).await?;
    board(name, &c, "0.001").await?;
    let v = first_vtxo(&c).await?;
    let (mut w, on) = bl(|| rgb_wallet(name, state))?;
    let asset = onchain_genesis_jump_in(regtest, &mut w, &on, v, state, rec).await?;
    rec(&format!("{name}_vtxo"), v.to_string());
    rec(&format!("{name}_asset"), asset.clone());
    let cid = ContractId::from_str(&asset).map_err(|e| anyhow!("{e}"))?;
    Ok((c, w, on, v, cid))
}

// --------------------------------------- multi-user
pub async fn run_multi() -> anyhow::Result<()> {
    let state = new_state("multi-user")?;
    let mut rec = summary_rec(state.clone());
    let regtest = Arc::new(Regtest::new());
    step("1. Alice (asset A) and Bob (asset B) colored VTXOs, Carol BTC-only");
    let (alice, arg, aon, va, cid_a) = colored_user("alice", &regtest, &state, &mut rec).await?;
    let (bob, brg, bon, vb, cid_b) = colored_user("bob", &regtest, &state, &mut rec).await?;
    let (carol, _) = ark_client("carol", regtest.clone(), &state).await?;
    board("carol", &carol, "0.0005").await?;
    let vc = first_vtxo(&carol).await?;
    let (dave, _) = ark_client("dave", regtest.clone(), &state).await?;
    let (frank, _) = ark_client("frank", regtest.clone(), &state).await?;
    let (mut drg, don) = bl(|| rgb_wallet("dave", &state))?;
    let (mut frg, fon) = bl(|| rgb_wallet("frank", &state))?;
    let (erg, eon) = bl(|| rgb_wallet("erin-verifier", &state))?;
    let ops = operators(&alice.server_info().await?);
    track("alice", &alice, &regtest, &arg, &aon, &ops, &state, &[va]).await?;
    track("bob", &bob, &regtest, &brg, &bon, &ops, &state, &[vb]).await?;
    rec("alice_ark_balance_A_start", bal_str(&ark_bal(&arg, &aon, &cid_a)?));
    rec("alice_db_balance_A_start", format!("{:?}", bl(|| arg.get_asset_balance(cid_a.to_string()))));

    step("2. off-chain send with change: Alice -> Dave 300 A + 20k sat; 700 A + BTC change to Alice");
    let (daddr, _) = dave.get_offchain_address().await?;
    let (achg, achg_v) = alice.get_offchain_address().await?;
    let hop = rgb_hop(
        "A->D", &alice, &arg, va, vec![SendReceiver::bitcoin(daddr, Amount::from_sat(SEND_SAT))], &achg, achg_v.owner_pk(), cid_a,
        vec![(1, SEND_ASSET, BL_D), (2, CHANGE_ASSET, BL_AC)], BL_CP, state.join("alice_to_dave.rgbc"),
    ).await?;
    let vd = OutPoint { txid: hop.ark_txid, vout: 1 };
    let va2 = OutPoint { txid: hop.ark_txid, vout: 2 };
    rec("hop_checkpoint", hop.checkpoint_txid.to_string());
    rec("hop_ark_tx", hop.ark_txid.to_string());
    wait_vtxo(&dave, vd, "dave").await?;
    wait_vtxo(&alice, va2, "alice").await?;
    let hop_vals: Vec<u64> = alice.network_client().get_virtual_txs(vec![hop.ark_txid.to_string()], None, None).await.map_err(|e| anyhow!("{e:?}"))?
        .txs[0].unsigned_tx.output.iter().map(|o| o.value.to_sat()).collect();
    rec("hop_ark_tx_output_values", format!("{hop_vals:?} (0: opret, 1: Dave, 2: Alice BTC+asset change, 3: anchor)"));
    let p_hop = proof_path(&hop.consignment);
    build_proof(&alice, &regtest, &[hop.checkpoint_txid, hop.ark_txid], &[], &p_hop).await?;
    add_vtxos(&alice, &regtest, &p_hop, &[vd, va2]).await?;
    {
        let _down = crate::proof::arkd_down(&mut rec, "dave_hop")?;
        import(&drg, &don, &p_hop, &ops)?;
        let (_, a) = bl(|| drg.accept_transfer_consignment_offchain(don.clone(), hop.consignment.clone(), vec![], hop.ark_txid.to_string(), 1, BL_D))?;
        rec("dave_hop_accept", format!("{a:?}"));
        let b = ark_bal(&drg, &don, &cid_a)?;
        rec("dave_ark_balance_A_after_hop", bal_str(&b));
        ensure!(b.active == SEND_ASSET, "dave hop balance");
        import(&arg, &aon, &p_hop, &ops)?;
        own(&arg, va2)?;
        let b = ark_bal(&arg, &aon, &cid_a)?;
        rec("alice_ark_balance_A_after_hop", bal_str(&b));
        ensure!(b.active == CHANGE_ASSET && b.in_flight == 0, "alice change balance");
    }

    step("3. ONE batch: Alice refresh (700 A), Dave refresh (300 A), Bob refresh -> Frank (1000 B), Carol BTC-only");
    let (a_to, _) = alice.get_offchain_address().await?;
    let (d_to, _) = dave.get_offchain_address().await?;
    let (f_to, _) = frank.get_offchain_address().await?;
    let (c_to, _) = carol.get_offchain_address().await?;
    let mut carol_body = |_: &ark_client::ForfeitHookContext<'_>, _: &mut Psbt| -> anyhow::Result<()> { Ok(()) };
    let (ra, rd, rb, rc) = tokio::join!(
        colored_batch("alice", &alice, &arg, cid_a, va2, CHANGE_ASSET, BatchOp::Refresh(a_to)),
        colored_batch("dave", &dave, &drg, cid_a, vd, SEND_ASSET, BatchOp::Refresh(d_to)),
        colored_batch("bob", &bob, &brg, cid_b, vb, ISSUE_AMOUNT, BatchOp::Refresh(f_to)),
        hooked_batch("carol", &carol, vc, BatchOp::Refresh(c_to), &mut carol_body, 1),
    );
    let (ca, fa) = ra?;
    let (cd, fd) = rd?;
    let (cb, fb) = rb?;
    let cc = rc?;
    rec("commitments", format!("alice {ca} dave {cd} bob {cb} carol {cc}"));
    let same = ca == cd && cd == cb && cb == cc;
    rec("same_batch", same.to_string());
    ensure!(same, "not one batch");
    mine_until(&regtest, &ca).await?;
    for (n, f) in [("alice", &fa), ("dave", &fd), ("bob", &fb)] {
        rec(&format!("{n}_forfeit"), format!("{} -> {}", f.forfeit_tx.compute_txid(), f.target));
    }
    let tree_outs: Vec<String> = carol.list_vtxos_filtered(ark_client::GetVtxosRequestFilter::Spendable).await?
        .spendable_offchain().map(|v| format!("{} {}", v.vtxo().outpoint, v.vtxo().amount)).collect();
    rec("carol_btc_vtxos_after", format!("{tree_outs:?}"));
    wait_vtxo(&frank, fb.target, "frank").await?;

    step("4. proofs, then every receiver validates with arkd PAUSED");
    let pa = forfeit_proof(&alice, &regtest, &fa, &[], &state.join("alice_refresh.rgbc")).await?;
    let pd = forfeit_proof(&dave, &regtest, &fd, &[p_hop.clone()], &state.join("dave_refresh.rgbc")).await?;
    let pb = forfeit_proof(&bob, &regtest, &fb, &[], &state.join("bob_to_frank.rgbc")).await?;
    for (n, p) in [("alice", &pa), ("dave", &pd), ("frank", &pb)] {
        rec(&format!("proof_{n}"), format!("{} B, {} PSBTs", crate::proof::file_len(p), crate::proof::psbt_txids(p)?.len()));
    }
    {
        let _down = crate::proof::arkd_down(&mut rec, "receivers")?;
        // sweep-check negative: a normal (not swept) forfeit declared as a sweep recovery
        let neg = copy_with(&pb, "declared_recovery_not_swept", |f| f.recoveries = vec![fb.forfeit_tx.compute_txid().to_string()])?;
        let r = import(&erg, &eon, &neg, &ops);
        rec("neg_recovery_declared_for_unswept_vtxo", format!("{:?}", r.as_ref().map_err(|e| e.to_string())).chars().take(300).collect());
        ensure!(r.is_err(), "unswept recovery accepted");
        // Frank: explicit seal on his new VTXO; must fail before the forfeit is attested
        let cons_f = state.join("bob_to_frank.rgbc");
        let r = bl(|| erg.validate_consignment_ark(eon.clone(), cons_f.clone(), true));
        rec("neg_frank_unattested_forfeit", r.is_err().to_string());
        ensure!(r.is_err(), "unattested forfeit accepted");
        let n = crate::proof::negatives("frank", &frg, &fon, &pb, &ops, &mut rec)?;
        ensure!(n, "mutated proof accepted");
        import(&frg, &fon, &pb, &ops)?;
        let got = bl(|| frg.ark_receive_vtxo_transfer(fon.clone(), cons_f.clone(), fb.target.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("frank_received_B", got.to_string());
        import(&drg, &don, &pd, &ops)?;
        own(&drg, fd.target)?;
        import(&arg, &aon, &pa, &ops)?;
        own(&arg, fa.target)?;
        import(&brg, &bon, &pb, &ops)?;
        for (n, w, o, c, exp) in [
            ("alice_A", &arg, &aon, &cid_a, CHANGE_ASSET),
            ("dave_A", &drg, &don, &cid_a, SEND_ASSET),
            ("frank_B", &frg, &fon, &cid_b, ISSUE_AMOUNT),
            ("bob_B", &brg, &bon, &cid_b, 0),
        ] {
            let b = ark_bal(w, o, c)?;
            rec(&format!("{n}_ark_balance"), bal_str(&b));
            ensure!(b.active == exp && b.in_flight == 0 && b.to_recover == 0, "{n} balance {} != {exp}", b.active);
        }
        let st = bl(|| frg.ark_vtxo_status(fon.clone(), fb.target.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("frank_vtxo_status", format!("{st:?}"));
    }
    rec("alice_db_balance_A_end (stock DB view, on-chain UTXOs only)", format!("{:?}", bl(|| arg.get_asset_balance(cid_a.to_string()))));
    let lt = bl(|| arg.list_ark_transfers()).map_err(|e| anyhow!("{e}"))?;
    rec("alice_ark_transfers", format!("{:?}", lt.iter().map(|t| format!("{} {} {:?} {}", &t.witness[..8], t.status, t.outputs, t.inputs.len())).collect::<Vec<_>>()));
    rec("PASS", "true".into());
    println!("\nMULTI-USER BATCH RESULT: PASS");
    Ok(())
}

// --------------------------------------- failed-batch
pub async fn run_fail() -> anyhow::Result<()> {
    let state = new_state("failed-batch")?;
    let mut rec = summary_rec(state.clone());
    let regtest = Arc::new(Regtest::new());
    step("1. Alice colored VTXO, Bob BTC VTXO, Gina receiver");
    let (alice, arg, aon, va, cid) = colored_user("alice", &regtest, &state, &mut rec).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    board("bob", &bob, "0.0005").await?;
    let vb = first_vtxo(&bob).await?;
    let (gina, _) = ark_client("gina", regtest.clone(), &state).await?;
    let (mut grg, gon) = bl(|| rgb_wallet("gina", &state))?;
    let ops = operators(&alice.server_info().await?);
    track("alice", &alice, &regtest, &arg, &aon, &ops, &state, &[va]).await?;

    step("2. attempt 1 - same batch, Bob refuses to sign his forfeit");
    let (g_to, _) = gina.get_offchain_address().await?;
    let (b_to, _) = bob.get_offchain_address().await?;
    let mut bob_body = |_: &ark_client::ForfeitHookContext<'_>, _: &mut Psbt| -> anyhow::Result<()> { bail!("bob goes offline before signing his forfeit") };
    let t0 = crate::now();
    let (ra, rb) = tokio::join!(
        colored_batch("alice-attempt1", &alice, &arg, cid, va, ISSUE_AMOUNT, BatchOp::Refresh(g_to.clone())),
        hooked_batch("bob-refuses", &bob, vb, BatchOp::Refresh(b_to), &mut bob_body, 1),
    );
    rec("attempt1_alice", format!("{:?}", ra.as_ref().map(|(c, f)| (c.to_string(), f.forfeit_tx.compute_txid().to_string())).map_err(|e| e.to_string())).chars().take(400).collect());
    rec("attempt1_bob", format!("{:?}", rb.as_ref().map_err(|e| e.to_string())).chars().take(300).collect());
    rec("attempt1_seconds", (crate::now() - t0).to_string());
    ensure!(ra.is_err(), "attempt 1 unexpectedly succeeded (arkd dropped Bob): no failed colored batch to test");
    let lt = bl(|| arg.list_ark_transfers()).map_err(|e| anyhow!("{e}"))?;
    let f1 = lt.iter().rev().find(|t| t.inputs.contains(&va.to_string()) && t.status == "pending").ok_or_else(|| anyhow!("no pending colored forfeit"))?.witness.clone();
    rec("attempt1_colored_forfeit (never co-signed)", f1.clone());
    let b = ark_bal(&arg, &aon, &cid)?;
    rec("alice_ark_balance_after_failure", bal_str(&b));
    let vi = vtxo_info(va).await.ok_or_else(|| anyhow!("indexer"))?;
    rec("v_indexer_after_failure", format!("spent={} spent_by={:?}", vi.is_spent, vi.spent_by));
    let f1_known = alice.network_client().get_virtual_txs(vec![f1.clone()], None, None).await.map(|r| r.txs.len()).unwrap_or(0);
    rec("f1_known_to_arkd", f1_known.to_string());

    step("3. roll back the never-signed colored forfeit in rgb-lib, retry (with retries)");
    bl(|| arg.ark_rollback_transfer(aon.clone(), f1.clone())).map_err(|e| anyhow!("rollback: {e}"))?;
    let b = ark_bal(&arg, &aon, &cid)?;
    rec("alice_ark_balance_after_rollback", bal_str(&b));
    ensure!(b.active == ISSUE_AMOUNT && b.in_flight == 0, "rollback did not restore the balance");
    let mut done = None;
    for attempt in 0..4 {
        match colored_batch(&format!("alice-retry{attempt}"), &alice, &arg, cid, va, ISSUE_AMOUNT, BatchOp::Refresh(g_to.clone())).await {
            Ok(x) => {
                done = Some(x);
                break;
            }
            Err(e) => {
                rec(&format!("retry{attempt}_error"), format!("{e:#}").chars().take(300).collect());
                let lt = bl(|| arg.list_ark_transfers()).map_err(|e| anyhow!("{e}"))?;
                if let Some(t) = lt.iter().rev().find(|t| t.inputs.contains(&va.to_string()) && t.status == "pending") {
                    bl(|| arg.ark_rollback_transfer(aon.clone(), t.witness.clone())).map_err(|e| anyhow!("rollback: {e}"))?;
                    rec(&format!("retry{attempt}_rolled_back"), t.witness.clone());
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
    }
    let (c2, f2) = done.ok_or_else(|| anyhow!("retries exhausted"))?;
    mine_until(&regtest, &c2).await?;
    let f2id = f2.forfeit_tx.compute_txid();
    rec("retry_commitment", c2.to_string());
    rec("retry_forfeit", f2id.to_string());
    rec("retry_target", f2.target.to_string());
    let vi = vtxo_info(va).await.ok_or_else(|| anyhow!("indexer"))?;
    rec("v_indexer_after_retry", format!("spent={} spent_by={:?} settled_by={:?}", vi.is_spent, vi.spent_by, vi.settled_by));
    wait_vtxo(&gina, f2.target, "gina").await?;

    step("4. Gina validates offline (arkd paused)");
    let cons = state.join("alice_to_gina.rgbc");
    let p = forfeit_proof(&alice, &regtest, &f2, &[], &cons).await?;
    {
        let _down = crate::proof::arkd_down(&mut rec, "gina")?;
        import(&grg, &gon, &p, &ops)?;
        let got = bl(|| grg.ark_receive_vtxo_transfer(gon.clone(), cons.clone(), f2.target.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("gina_received", got.to_string());
        let b = ark_bal(&grg, &gon, &cid)?;
        rec("gina_ark_balance", bal_str(&b));
        ensure!(b.active == ISSUE_AMOUNT, "gina balance");
        import(&arg, &aon, &p, &ops)?;
        let b = ark_bal(&arg, &aon, &cid)?;
        rec("alice_ark_balance_end", bal_str(&b));
        ensure!(b.active == 0 && b.in_flight == 0, "alice end balance");
        let r = bl(|| arg.ark_rollback_transfer(aon.clone(), f2id.to_string()));
        rec("neg_rollback_of_attested_forfeit", format!("{:?}", r.as_ref().map_err(|e| e.to_string())));
        ensure!(r.is_err(), "rolled back an attested forfeit");
    }
    let lt = bl(|| arg.list_ark_transfers()).map_err(|e| anyhow!("{e}"))?;
    rec("alice_ark_transfers", format!("{:?}", lt.iter().map(|t| format!("{} {}", &t.witness[..8], t.status)).collect::<Vec<_>>()));
    rec("PASS", "true".into());
    println!("\nFAILED-BATCH RETRY RESULT: PASS");
    Ok(())
}

// --------------------------------------- fraud
fn arkdb(mode: &str, sql: &str) -> anyhow::Result<String> {
    let o = std::process::Command::new("docker")
        .args(["run", "--rm", "-v", "arkade-regtest_ark_datadir:/app/data", "-v", &format!("{}:/t", crate::paths::tools().display()), "busybox", "/t/arkdb", "/app/data/db/sqlite.db", mode, sql])
        .output()?;
    let s = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    ensure!(o.status.success(), "arkdb: {s}");
    Ok(s.trim().to_string())
}

pub async fn run_fraud() -> anyhow::Result<()> {
    let state = new_state("fraud")?;
    let mut rec = summary_rec(state.clone());
    let regtest = Arc::new(Regtest::new());
    step("1. Alice colored VTXO V; receivers Dave and Erin; judge");
    let (alice, arg, aon, va, cid) = colored_user("alice", &regtest, &state, &mut rec).await?;
    let (dave, _) = ark_client("dave", regtest.clone(), &state).await?;
    let (erin, _) = ark_client("erin", regtest.clone(), &state).await?;
    let (mut drg, don) = bl(|| rgb_wallet("dave", &state))?;
    let (mut erg, eon) = bl(|| rgb_wallet("erin", &state))?;
    let (jrg, jon) = bl(|| rgb_wallet("judge", &state))?;
    let ops = operators(&alice.server_info().await?);
    track("alice", &alice, &regtest, &arg, &aon, &ops, &state, &[va]).await?;

    step("2. batch 1: V -> Dave (forfeit F1); Dave validates offline");
    let (d_to, _) = dave.get_offchain_address().await?;
    let (c1, f1) = colored_batch("alice->dave", &alice, &arg, cid, va, ISSUE_AMOUNT, BatchOp::Refresh(d_to)).await?;
    mine_until(&regtest, &c1).await?;
    let f1id = f1.forfeit_tx.compute_txid();
    rec("batch1_commitment", c1.to_string());
    rec("f1", f1id.to_string());
    wait_vtxo(&dave, f1.target, "dave").await?;
    let cons1 = state.join("alice_to_dave.rgbc");
    let p1 = forfeit_proof(&alice, &regtest, &f1, &[], &cons1).await?;
    {
        let _d = crate::proof::arkd_down(&mut rec, "dave")?;
        import(&drg, &don, &p1, &ops)?;
        let got = bl(|| drg.ark_receive_vtxo_transfer(don.clone(), cons1.clone(), f1.target.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("dave_received", got.to_string());
    }
    let vi = vtxo_info(va).await.ok_or_else(|| anyhow!("indexer"))?;
    rec("v_indexer_after_batch1", format!("spent={} spent_by={:?}", vi.is_spent, vi.spent_by));
    ensure!(vi.spent_by.map(|t| t.to_string()) == Some(f1id.to_string()), "spent_by != F1");

    step("3. honest arkd refuses a second forfeit of V");
    let (e_to, _) = erin.get_offchain_address().await?;
    let r = colored_batch("alice->erin-honest", &alice, &arg, cid, va, ISSUE_AMOUNT, BatchOp::Refresh(e_to.clone())).await;
    rec("honest_operator_second_forfeit", format!("{:?}", r.as_ref().map(|(c, _)| c.to_string()).map_err(|e| e.to_string())).chars().take(300).collect());
    ensure!(r.is_err(), "honest arkd accepted a second forfeit");
    let lt = bl(|| arg.list_ark_transfers()).map_err(|e| anyhow!("{e}"))?;
    if let Some(t) = lt.iter().rev().find(|t| t.status == "pending" && t.inputs.contains(&va.to_string()) && t.witness != f1id.to_string()) {
        bl(|| arg.ark_rollback_transfer(aon.clone(), t.witness.clone())).map_err(|e| anyhow!("{e}"))?;
        rec("honest_attempt_rolled_back", t.witness.clone());
    }

    step("4. COLLUSION: the operator marks V unspent in its own DB; Alice signs a second refresh -> Erin");
    let w = format!("txid='{}' AND vout={}", va.txid, va.vout);
    rec("arkdb_before", arkdb("query", &format!("SELECT spent, spent_by, settled_by FROM vtxo WHERE {w}"))?);
    rec("arkdb_edit", arkdb("exec", &format!("UPDATE vtxo SET spent=0, spent_by='', settled_by='' WHERE {w}"))?);
    rec("arkdb_after", arkdb("query", &format!("SELECT spent, spent_by, settled_by FROM vtxo WHERE {w}"))?);
    let mut res = None;
    for attempt in 0..3 {
        match colored_batch(&format!("alice->erin-collusion{attempt}"), &alice, &arg, cid, va, ISSUE_AMOUNT, BatchOp::Refresh(e_to.clone())).await {
            Ok(x) => {
                res = Some(x);
                break;
            }
            Err(e) => {
                rec(&format!("collusion_attempt{attempt}_error"), format!("{e:#}").chars().take(300).collect());
                let lt = bl(|| arg.list_ark_transfers()).map_err(|e| anyhow!("{e}"))?;
                if let Some(t) = lt.iter().rev().find(|t| t.status == "pending" && t.inputs.contains(&va.to_string()) && t.witness != f1id.to_string()) {
                    bl(|| arg.ark_rollback_transfer(aon.clone(), t.witness.clone())).map_err(|e| anyhow!("{e}"))?;
                }
                if attempt == 1 {
                    // arkd may cache the VTXO: restart it (the operator controls its own process)
                    rec("arkd_restart_for_collusion", "true".into());
                    let _ = std::process::Command::new("docker").args(["restart", "arkd"]).status();
                    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                    rec("arkdb_reedit", arkdb("exec", &format!("UPDATE vtxo SET spent=0, spent_by='', settled_by='' WHERE {w}"))?);
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
    let (c2, f2) = res.ok_or_else(|| anyhow!("colluding second batch failed"))?;
    mine_until(&regtest, &c2).await?;
    let f2id = f2.forfeit_tx.compute_txid();
    rec("batch2_commitment", c2.to_string());
    rec("f2", f2id.to_string());
    wait_vtxo(&erin, f2.target, "erin").await?;
    let cons2 = state.join("alice_to_erin.rgbc");
    let p2 = forfeit_proof(&alice, &regtest, &f2, &[], &cons2).await?;

    step("5. Erin validates offline: NOT prevented (both receivers hold the 1000)");
    let fraud_path = state.join("fraud-proof.json");
    {
        let _d = crate::proof::arkd_down(&mut rec, "erin+judge")?;
        import(&erg, &eon, &p2, &ops)?;
        let got = bl(|| erg.ark_receive_vtxo_transfer(eon.clone(), cons2.clone(), f2.target.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("erin_received (offline, double spend not prevented)", got.to_string());
        let bd = ark_bal(&drg, &don, &cid)?;
        let be = ark_bal(&erg, &eon, &cid)?;
        rec("dave_ark_balance", bal_str(&bd));
        rec("erin_ark_balance", bal_str(&be));

        step("6. fraud proof: two operator-co-signed forfeits of V (judge, chain only)");
        let load = |p: &Path| -> anyhow::Result<ArkProof> { Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?) };
        let fp = ArkFraudProof { version: 1, vtxo: va.to_string(), forfeit_a: f1id.to_string(), proof_a: load(&p1)?, forfeit_b: f2id.to_string(), proof_b: load(&p2)? };
        std::fs::write(&fraud_path, serde_json::to_string_pretty(&fp)?)?;
        rec("fraud_proof", format!("{} ({} B)", fraud_path.display(), crate::proof::file_len(&fraud_path)));
        let ev = bl(|| jrg.verify_ark_fraud_proof(jon.clone(), fraud_path.clone(), ops.clone())).map_err(|e| anyhow!("{e}"))?;
        rec("fraud_evidence", format!("{ev:?}"));
        // negatives
        let mut same = fp.clone();
        same.forfeit_b = same.forfeit_a.clone();
        same.proof_b = same.proof_a.clone();
        let mut stripped = fp.clone();
        stripped.proof_b.psbts = stripped.proof_b.psbts.iter().map(|b| {
            let mut p = Psbt::from_str(b).unwrap();
            if p.unsigned_tx.compute_txid() == f2id {
                let opk: Vec<bitcoin::XOnlyPublicKey> = ops.split(',').filter_map(|k| bitcoin::PublicKey::from_str(k).ok()).map(|k| k.inner.x_only_public_key().0).collect();
                for i in p.inputs.iter_mut() {
                    i.tap_script_sigs.retain(|(pk, _), _| !opk.contains(pk));
                }
            }
            p.to_string()
        }).collect();
        let mut other = fp.clone();
        other.vtxo = f1.target.to_string();
        for (n, f) in [("same_forfeit_twice", same), ("second_forfeit_without_operator_sig", stripped), ("forfeits_do_not_spend_claimed_vtxo", other)] {
            let path = state.join(format!("fraud-neg-{n}.json"));
            std::fs::write(&path, serde_json::to_string(&f)?)?;
            let r = bl(|| jrg.verify_ark_fraud_proof(jon.clone(), path.clone(), ops.clone()));
            rec(&format!("neg_fraud_{n}"), format!("{:?}", r.as_ref().map(|_| "ACCEPTED").map_err(|e| e.to_string())).chars().take(250).collect());
            ensure!(r.is_err(), "{n} accepted");
        }
    }

    step("7. online detection via the indexer (spent_by)");
    let vi = vtxo_info(va).await.ok_or_else(|| anyhow!("indexer"))?;
    let sb = vi.spent_by.map(|t| t.to_string()).unwrap_or_default();
    let dave_alarm = sb != f1id.to_string();
    let erin_alarm = sb != f2id.to_string();
    rec("v_indexer_now", format!("spent={} spent_by={sb} settled_by={:?}", vi.is_spent, vi.settled_by));
    rec("dave_online_check (holds F1)", if dave_alarm { format!("CONFLICT DETECTED: indexer says V spent by {sb}, not by my forfeit {f1id}") } else { "consistent".into() });
    rec("erin_online_check (holds F2)", if erin_alarm { "CONFLICT DETECTED".into() } else { "consistent (the later receiver cannot tell from spent_by alone)".into() });
    ensure!(dave_alarm, "online check did not detect the conflict");
    rec("PASS", "true".into());
    println!("\nFRAUD RESULT: PASS (detected, not prevented)");
    Ok(())
}

// ------------------------------------------------------------------ on-chain sweep check on sweep-recovery data
pub async fn verify_recovery(dir: PathBuf) -> anyhow::Result<()> {
    let out = dir.join("verify-recovery");
    std::fs::create_dir_all(&out)?;
    let mut rec = summary_rec(out.clone());
    let ret: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("return/summary.json"))?)?;
    let fid = Txid::from_str(ret["recovery_forfeit"].as_str().ok_or_else(|| anyhow!("no recovery_forfeit"))?)?;
    let efid = Txid::from_str(ret["exit_forfeit"].as_str().ok_or_else(|| anyhow!("no exit_forfeit"))?)?;
    let (vw, von) = bl(|| rgb_wallet("verifier", &out))?;
    let regtest = Arc::new(Regtest::new());
    let (c, _) = ark_client("verifier", regtest.clone(), &out).await?;
    let ops = operators(&c.server_info().await?);
    let rp = proof_path(&dir.join("return/recovery_forfeit.rgbc"));
    let ep = proof_path(&dir.join("return/exit_forfeit.rgbc"));
    let pos = copy_with(&rp, "s6-recovery-declared", |_| {})?;
    mark_recovery(&pos, fid)?;
    let neg = copy_with(&ep, "s6-unswept-declared", |_| {})?;
    mark_recovery(&neg, efid)?;
    let _down = crate::proof::arkd_down(&mut rec, "verifier")?;
    let r = import(&vw, &von, &pos, &ops);
    rec("recovery_forfeit", fid.to_string());
    rec("recovery_declared_swept_check", format!("{:?}", r.as_ref().map_err(|e| e.to_string())));
    let r2 = import(&vw, &von, &neg, &ops);
    rec("neg_exit_forfeit_declared_as_recovery", format!("{:?}", r2.as_ref().map_err(|e| e.to_string())).chars().take(300).collect());
    ensure!(r.is_ok() && r2.is_err(), "sweep check");
    rec("PASS", "true".into());
    println!("\nSWEEP-CHECK ON RECOVERY DATA RESULT: PASS");
    Ok(())
}

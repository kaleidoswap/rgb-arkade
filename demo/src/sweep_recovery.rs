//! Scenarios `sweep-recovery-*`: sweep recovery of a colored VTXO.
//!
//! A user offline for months: the batch expires and is swept, nobody acted. On return the
//! swept (recoverable) VTXO V is settled into a new batch like stock Arkade does for BTC, but with
//! the arkd recovery-forfeit change the operator requires and co-signs a forfeit for V, so the forfeit
//! carries the RGB transition V -> V' (new VTXO), exactly like a refresh.
//! `scenario sweep-recovery-setup <dir>` / `scenario sweep-recovery-return <dir>`; the orchestrator shortens and restores the
//! batch expiry and waits for the sweep in between (wallet processes stopped).
use crate::long_offline::{colored_health, pay_from_rgb_output, receive, vtxo_info};
use crate::proof::{build_proof, import, operators, proof_path};
use crate::forfeit::{BatchOp, colored_batch, first_vtxo, mine_until, summary_rec};
use crate::{ISSUE_AMOUNT, ark_client, ark_client_load, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, rgb_wallet_load, step};
use anyhow::{anyhow, ensure};
use ark_core::intent::{self, IntentMessage, RGB_RECOVERY_FORFEIT};
use bitcoin::key::Keypair;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use bitcoin::{Address, Amount, OutPoint, Psbt, ScriptBuf, TxOut, Txid};
use regtest::Regtest;
use rgb_lib::wallet::RgbWalletOpsOffline;
use rgb_lib::{Assignment, ContractId, FileContent};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

const EXIT_SAT: u64 = 20_000;

fn scen(dir: &Path) -> PathBuf {
    dir.join("scenario.json")
}

pub async fn setup(dir: PathBuf) -> anyhow::Result<()> {
    let state = dir.join("users");
    std::fs::create_dir_all(&state)?;
    std::fs::create_dir_all(dir.join("setup"))?;
    let mut rec = summary_rec(dir.join("setup"));
    let regtest = Arc::new(Regtest::new());
    step("1. Alice boards, on-chain genesis + jump-in to V");
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (_m, _) = ark_client("mallory", regtest.clone(), &state).await?;
    bl(|| rgb_wallet("dave", &state))?;
    board("alice", &alice, "0.001").await?;
    let v = first_vtxo(&alice).await?;
    let vi = vtxo_info(v).await.ok_or_else(|| anyhow!("no indexer info"))?;
    let left = vi.expires_at - now() as i64;
    rec("alice_vtxo", format!("{v} expires_at {} ({left}s) batch {:?}", vi.expires_at, vi.commitment_txids));
    ensure!(left < 700, "expiry not shortened: set vtxoTreeExpiry via the admin API");
    let (mut w, online) = bl(|| rgb_wallet("alice", &state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut w, &online, v, &state, &mut rec).await?;
    rec("asset_id", asset_id.clone());
    // track V in rgb-lib (branch from arkd, chain-verified) for the Ark-aware status
    let ops = operators(&alice.server_info().await?);
    let tp = dir.join("setup/track-v.arkproof.pb");
    crate::proof::add_vtxos(&alice, &regtest, &tp, &[v]).await?;
    import(&w, &online, &tp, &ops)?;
    crate::batch_scenarios::own(&w, v)?;
    let st = bl(|| w.ark_vtxo_status(online.clone(), v.to_string()));
    rec("rgb_lib_ark_status_before_offline", format!("{st:?}"));
    let b = bl(|| w.get_asset_balance_ark(online.clone(), asset_id.clone()));
    rec("rgb_lib_ark_balance_before_offline", format!("{b:?}"));
    let s = serde_json::json!({"state": state.display().to_string(), "vtxo": v.to_string(), "asset_id": asset_id, "expires_at": vi.expires_at, "t_offline": now()});
    std::fs::write(scen(&dir), serde_json::to_string_pretty(&s)?)?;
    println!("\nSWEEP RECOVERY SETUP DONE: alice goes offline");
    Ok(())
}

/// Mallory forges Alice's signature on a recovery intent for V (signs with her own key under
/// Alice's/operator's pubkeys).
fn forged_intent(inputs: Vec<intent::Input>, to: ScriptBuf, amount: Amount) -> anyhow::Result<intent::Intent> {
    let secp = Secp256k1::new();
    let kp = Keypair::from_secret_key(&secp, &SecretKey::new(&mut rand::thread_rng()));
    let sign_v = |input: &mut bitcoin::psbt::Input, msg: bitcoin::secp256k1::Message| -> Result<Vec<(bitcoin::secp256k1::schnorr::Signature, bitcoin::XOnlyPublicKey)>, ark_core::Error> {
        let script = input.witness_script.clone().ok_or_else(|| ark_core::Error::ad_hoc("no witness script"))?;
        Ok(crate::bench::checksig_pks(&script).into_iter().map(|pk| (secp.sign_schnorr_no_aux_rand(&msg, &kp), pk)).collect())
    };
    let sign_o = |_: &mut bitcoin::psbt::Input, _: bitcoin::secp256k1::Message| -> Result<(bitcoin::secp256k1::schnorr::Signature, bitcoin::XOnlyPublicKey), ark_core::Error> {
        Err(ark_core::Error::ad_hoc("no onchain inputs"))
    };
    let t = now();
    let msg = IntentMessage::Register { onchain_output_indexes: vec![], valid_at: t, expire_at: t + 120, own_cosigner_pks: vec![kp.public_key()] };
    Ok(intent::make_intent(sign_v, sign_o, inputs, vec![intent::Output::Offchain(TxOut { value: amount, script_pubkey: to })], msg)?)
}

fn mutate(src: &Path, name: &str, fid: Txid, ops: &str, f: impl Fn(&mut Psbt, &[bitcoin::XOnlyPublicKey])) -> anyhow::Result<PathBuf> {
    let mut pf = crate::proof::load(&src)?;
    let opks: Vec<bitcoin::XOnlyPublicKey> = ops.split(',').map(|k| bitcoin::PublicKey::from_str(k).map(|p| p.inner.x_only_public_key().0)).collect::<Result<_, _>>()?;
    for (i, b) in pf.psbts.clone().iter().enumerate() {
        let mut p = Psbt::from_str(b)?;
        if p.unsigned_tx.compute_txid() == fid {
            f(&mut p, &opks);
            let nt = p.unsigned_tx.compute_txid();
            pf.psbts[i] = p.to_string();
            for w in pf.witnesses.iter_mut() {
                if *w == fid.to_string() {
                    *w = nt.to_string();
                }
            }
        }
    }
    let out = src.with_extension(format!("neg-{name}.pb"));
    crate::proof::save(&out, &pf)?;
    Ok(out)
}

pub async fn return_user(dir: PathBuf) -> anyhow::Result<()> {
    let s: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(scen(&dir))?)?;
    let state = PathBuf::from(s["state"].as_str().unwrap());
    let v = OutPoint::from_str(s["vtxo"].as_str().unwrap())?;
    let asset_id = s["asset_id"].as_str().unwrap().to_string();
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    let out = dir.join("return");
    std::fs::create_dir_all(&out)?;
    let mut rec = summary_rec(out.clone());
    let regtest = Arc::new(Regtest::new());
    let off = now() as i64 - s["t_offline"].as_i64().unwrap();
    rec("offline_for", format!("{off} s"));
    step("2. Alice comes back (new process); her colored VTXO was swept");
    let (alice, _) = ark_client_load("alice", regtest.clone(), &state).await?;
    let (mallory, _) = ark_client_load("mallory", regtest.clone(), &state).await?;
    let (mut rgb, online) = bl(|| rgb_wallet_load("alice", &state))?;
    let health = colored_health(v, &regtest).await;
    rec("health_on_return", health.clone());
    // rgb-lib's own status (chain only), not the demo health check
    let st = bl(|| rgb.ark_vtxo_status(online.clone(), v.to_string()));
    rec("rgb_lib_ark_status_on_return", format!("{st:?}"));
    let b6 = bl(|| rgb.get_asset_balance_ark(online.clone(), asset_id.clone())).map_err(|e| anyhow!("{e}"))?;
    rec("rgb_lib_ark_balance_on_return", format!("active {} to_recover {} in_flight {}", b6.active, b6.to_recover, b6.in_flight));
    rec("rgb_lib_db_balance_on_return (stock view, stale)", format!("{:?}", bl(|| rgb.get_asset_balance(asset_id.clone()))));
    ensure!(b6.to_recover == ISSUE_AMOUNT && b6.active == 0, "rgb-lib does not flag the swept VTXO as to-recover");
    let vi = vtxo_info(v).await.ok_or_else(|| anyhow!("indexer"))?;
    rec("v_indexer_before", format!("swept={} spent={} expires_at={} spent_by={:?}", vi.is_swept, vi.is_spent, vi.expires_at, vi.spent_by));
    ensure!(vi.is_swept, "V not swept yet");
    let ops = operators(&alice.server_info().await?);
    let amount = vi.amount;

    step("3. NEG non-owner recovery intent (Mallory forges Alice's signature)");
    RGB_RECOVERY_FORFEIT.store(true, Ordering::SeqCst);
    let inputs = alice.rgb_settleable_inputs(&[v]).await.map_err(|e| anyhow!("settleable: {e:?}"))?;
    rec("v_settleable_as_recoverable", format!("{} input(s), is_swept flag sent: {}", inputs.len(), inputs.first().map(|i| i.is_swept()).unwrap_or(true)));
    ensure!(inputs.len() == 1, "V not offered as recoverable");
    let (maddr, _) = mallory.get_offchain_address().await?;
    let forged = forged_intent(inputs.clone(), maddr.to_p2tr_script_pubkey(), amount)?;
    let r = mallory.network_client().register_intent(forged).await;
    rec("neg_non_owner_intent", format!("{:?}", r.as_ref().map_err(|e| format!("{e:?}"))).chars().take(300).collect());
    ensure!(r.is_err(), "forged intent accepted");

    step("4. Alice recovers V into a new batch; the forfeit carries T: V -> V'");
    let (addr, _) = alice.get_offchain_address().await?;
    let res = colored_batch("recover", &alice, &rgb, cid, v, ISSUE_AMOUNT, BatchOp::Refresh(addr.clone())).await;
    RGB_RECOVERY_FORFEIT.store(false, Ordering::SeqCst);
    let (c1, fc) = res?;
    mine_until(&regtest, &c1).await?;
    let fid = fc.forfeit_tx.compute_txid();
    rec("recovery_commitment", c1.to_string());
    rec("recovery_forfeit", fid.to_string());
    rec("recovered_vtxo", fc.target.to_string());
    let vi2 = vtxo_info(v).await.ok_or_else(|| anyhow!("indexer"))?;
    rec("v_indexer_after", format!("swept={} spent={} spent_by={:?} settled_by={:?}", vi2.is_swept, vi2.is_spent, vi2.spent_by, vi2.settled_by));
    rec("new_vtxo_health", colored_health(fc.target, &regtest).await);

    step("5. NEG second recovery intent for the same V (genuinely signed by Alice)");
    RGB_RECOVERY_FORFEIT.store(true, Ordering::SeqCst);
    let t = now();
    let (ck, _) = (Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::new(&mut rand::thread_rng())), 0);
    let msg = IntentMessage::Register { onchain_output_indexes: vec![], valid_at: t, expire_at: t + 120, own_cosigner_pks: vec![ck.public_key()] };
    let second = alice.rgb_sign_intent(inputs.clone(), vec![intent::Output::Offchain(TxOut { value: amount, script_pubkey: addr.to_p2tr_script_pubkey() })], msg);
    RGB_RECOVERY_FORFEIT.store(false, Ordering::SeqCst);
    let r = match second {
        Ok(i) => alice.network_client().register_intent(i).await.map_err(|e| format!("{e:?}")),
        Err(e) => Err(format!("signing: {e:?}")),
    };
    rec("neg_second_recovery_intent", format!("{r:?}").chars().take(300).collect());
    ensure!(r.is_err(), "second recovery intent accepted");

    step("6. Alice's own proof for the recovery forfeit (from arkd, after the sweep)");
    let own = out.join("recovery_forfeit.rgbc");
    fc.transfer.save_file(&own).map_err(|e| anyhow!("save: {e}"))?;
    let st = build_proof(&alice, &regtest, &[fid], &[], &proof_path(&own)).await?;
    // declare the recovery (receivers check the sweep on-chain) and track V'
    crate::proof::mark_recovery(&proof_path(&own), fid)?;
    crate::proof::add_vtxos(&alice, &regtest, &proof_path(&own), &[fc.target]).await?;
    rec("recovery_proof", format!("{} B, {} PSBTs (+ V' branch), recoveries [{fid}]", st.bytes, st.n_psbts));
    import(&rgb, &online, &proof_path(&own), &ops)?;
    crate::batch_scenarios::own(&rgb, fc.target)?;
    let _ = bl(|| rgb.update_witnesses(online.clone(), 0, vec![]));
    let bal = bl(|| rgb.get_asset_balance(asset_id.clone()));
    rec("alice_balance_after_recovery", format!("{bal:?}"));
    let b6 = bl(|| rgb.get_asset_balance_ark(online.clone(), asset_id.clone())).map_err(|e| anyhow!("{e}"))?;
    rec("rgb_lib_ark_balance_after_recovery", format!("active {} to_recover {} in_flight {} vtxos {:?}", b6.active, b6.to_recover, b6.in_flight, b6.vtxos));
    ensure!(b6.active == ISSUE_AMOUNT && b6.to_recover == 0, "rgb-lib balance not restored after recovery");

    step("7. Alice spends: collaborative exit of V' and on-chain payment to Dave");
    let exit_addr = Address::from_str(&bl(|| rgb.get_address())?)?.assume_checked();
    let (c2, fc2) = colored_batch("exit", &alice, &rgb, cid, fc.target, ISSUE_AMOUNT, BatchOp::Exit(exit_addr, Amount::from_sat(EXIT_SAT))).await?;
    mine_until(&regtest, &c2).await?;
    let fid2 = fc2.forfeit_tx.compute_txid();
    rec("exit_commitment", c2.to_string());
    rec("exit_forfeit", fid2.to_string());
    let own2 = out.join("exit_forfeit.rgbc");
    fc2.transfer.save_file(&own2).map_err(|e| anyhow!("save: {e}"))?;
    build_proof(&alice, &regtest, &[fid, fid2], &[proof_path(&own)], &proof_path(&own2)).await?;
    import(&rgb, &online, &proof_path(&own2), &ops)?;
    let _ = bl(|| rgb.update_witnesses(online.clone(), 0, vec![]));
    let (mut dave, dave_on) = bl(|| rgb_wallet_load("dave", &state))?;
    let recv = bl(|| dave.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
    let to = rgb_lib::utils::script_buf_from_recipient_id(recv.recipient_id.clone())?.ok_or_else(|| anyhow!("not witness"))?;
    let cons = out.join("alice_to_dave.rgbc");
    let ad = pay_from_rgb_output("alice", &mut rgb, &online, &regtest, fc2.target, EXIT_SAT, cid, ISSUE_AMOUNT, to, 7777, &cons).await?;
    rec("alice_to_dave_txid", ad.to_string());
    let st = build_proof(&alice, &regtest, &[fid, fid2, ad], &[proof_path(&own2)], &proof_path(&cons)).await?;
    let pf = crate::proof::load(&proof_path(&cons))?;
    rec("proof_ad_recoveries_carried", format!("{:?}", pf.recoveries));
    rec("proof_ad", format!("{} B, {} PSBTs, txids {:?}", st.bytes, st.n_psbts, crate::proof::psbt_txids(&proof_path(&cons))?));

    step("8. Dave validates with arkd PAUSED; negatives first");
    let _down = crate::proof::arkd_down(&mut rec, "dave")?;
    let neg = crate::proof::negatives("dave", &dave, &dave_on, &proof_path(&cons), &ops, &mut rec)?;
    ensure!(neg, "a mutated proof was accepted");
    // operator tries to move the asset elsewhere: rewrite the RGB commitment of the recovery forfeit
    let p1 = mutate(&proof_path(&cons), "operator_rewrites_commitment", fid, &ops, |p, _| {
        let mut b = [0u8; 32];
        b[0] = 0xee;
        p.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return(b);
    })?;
    // operator-only recovery: drop the owner's signature, keep the operator's
    let p2 = mutate(&proof_path(&cons), "operator_only_signature", fid, &ops, |p, opks| {
        for i in p.inputs.iter_mut() {
            i.tap_script_sigs.retain(|(pk, _), _| opks.contains(pk));
        }
    })?;
    for (n, p) in [("operator_rewrites_commitment", p1), ("operator_only_signature", p2)] {
        let r = import(&dave, &dave_on, &p, &ops);
        rec(&format!("neg_{n}"), format!("{:?}", r.as_ref().map_err(|e| e.to_string())).chars().take(300).collect());
        ensure!(r.is_err(), "{n} accepted");
    }
    let settled = receive("dave", &mut dave, &dave_on, &cons, &ops, &asset_id, &mut rec).await?;
    rec("PASS", (settled == ISSUE_AMOUNT).to_string());
    ensure!(settled == ISSUE_AMOUNT, "dave not settled");
    println!("\nSWEEP RECOVERY RESULT: PASS");
    Ok(())
}

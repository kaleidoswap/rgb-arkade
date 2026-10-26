//! Stage 4 / Workstream B: out-of-band payload size and validation time.
//!
//! `--bench <dir> <phase> [n]` with phase in: hops (n off-chain hops A<->B), refresh (n refreshes),
//! merge (k on-chain inputs from k senders merged into one payment), onchain (n mined hops).
//! Every measurement row goes to `<dir>/<phase>.jsonl`. With `--pause` arkd is paused while the
//! verifier wallets validate (proof verification never talks to arkd).
//! Also implements the compact binary proof (`.arkproof.bin`): only what the verifier reads
//! (unsigned tx, key-path sig, the one signed leaf + control block + sigs in script key order).
use crate::proof::{ProofFile, build_proof, file_len, import, operators, proof_path};
use crate::stage1::{rgb_hop, wait_vtxo};
use crate::stage2::{BatchOp, colored_batch, first_vtxo, mine_until};
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bare_opret, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_core::send::SendReceiver;
use bitcoin::taproot::{ControlBlock, LeafVersion, TapLeafHash};
use bitcoin::{Address, Amount, OutPoint, Psbt, ScriptBuf, Transaction, TxOut, Txid, XOnlyPublicKey};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::{Online, Outpoint, RgbWalletOpsOffline, RgbWalletOpsOnline, Wallet as RgbWallet};
use rgb_lib::{Assignment, ContractId, FileContent};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn pause_mode() -> bool {
    std::env::args().any(|a| a == "--pause")
}

// ---------- compact binary proof ----------
fn put_var(out: &mut Vec<u8>, n: usize) {
    bitcoin::consensus::encode::VarInt(n as u64).consensus_encode_to(out);
}
trait EncTo {
    fn consensus_encode_to(&self, out: &mut Vec<u8>);
}
impl EncTo for bitcoin::consensus::encode::VarInt {
    fn consensus_encode_to(&self, out: &mut Vec<u8>) {
        use bitcoin::consensus::Encodable;
        self.consensus_encode(out).unwrap();
    }
}
fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_var(out, b.len());
    out.extend_from_slice(b);
}
struct Rd<'a>(&'a [u8]);
impl Rd<'_> {
    fn var(&mut self) -> anyhow::Result<usize> {
        use bitcoin::consensus::Decodable;
        let mut c = std::io::Cursor::new(self.0);
        let v = bitcoin::consensus::encode::VarInt::consensus_decode(&mut c)?;
        self.0 = &self.0[c.position() as usize..];
        Ok(v.0 as usize)
    }
    fn take(&mut self, n: usize) -> anyhow::Result<Vec<u8>> {
        ensure!(self.0.len() >= n, "truncated");
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a.to_vec())
    }
    fn bytes(&mut self) -> anyhow::Result<Vec<u8>> {
        let n = self.var()?;
        self.take(n)
    }
}

pub(crate) fn checksig_pks(script: &ScriptBuf) -> Vec<XOnlyPublicKey> {
    use bitcoin::opcodes::all::{OP_CHECKSIG, OP_CHECKSIGVERIFY};
    use bitcoin::script::Instruction;
    let ins: Vec<_> = script.instructions().filter_map(|i| i.ok()).collect();
    ins.windows(2)
        .filter_map(|w| match (&w[0], &w[1]) {
            (Instruction::PushBytes(b), Instruction::Op(op)) if (*op == OP_CHECKSIG || *op == OP_CHECKSIGVERIFY) && b.len() == 32 => {
                XOnlyPublicKey::from_slice(b.as_bytes()).ok()
            }
            _ => None,
        })
        .collect()
}

/// Encode a v1 JSON proof into the compact binary form.
pub(crate) fn compact_encode(p: &ProofFile) -> anyhow::Result<Vec<u8>> {
    let mut out = vec![b'A', b'P', 2];
    put_var(&mut out, p.witnesses.len());
    for w in &p.witnesses {
        out.extend_from_slice(&bitcoin::consensus::encode::serialize(&Txid::from_str(w)?));
    }
    put_var(&mut out, p.psbts.len());
    for b in &p.psbts {
        let psbt = Psbt::from_str(b)?;
        put_bytes(&mut out, &bitcoin::consensus::encode::serialize(&psbt.unsigned_tx));
        for inp in &psbt.inputs {
            if let Some(sig) = inp.tap_key_sig {
                out.push(1);
                put_bytes(&mut out, &sig.to_vec());
            } else if !inp.tap_script_sigs.is_empty() {
                // the one fully signed leaf
                let mut done = false;
                for (cb, (script, ver)) in &inp.tap_scripts {
                    let leaf = TapLeafHash::from_script(script, *ver);
                    let pks = checksig_pks(script);
                    if pks.is_empty() || !pks.iter().all(|pk| inp.tap_script_sigs.contains_key(&(*pk, leaf))) {
                        continue;
                    }
                    out.push(2);
                    put_bytes(&mut out, script.as_bytes());
                    put_bytes(&mut out, &cb.serialize());
                    for pk in &pks {
                        put_bytes(&mut out, &inp.tap_script_sigs[&(*pk, leaf)].to_vec());
                    }
                    done = true;
                    break;
                }
                ensure!(done, "no fully signed leaf");
            } else {
                out.push(0);
            }
        }
    }
    Ok(out)
}

pub(crate) fn compact_decode(b: &[u8]) -> anyhow::Result<ProofFile> {
    let mut r = Rd(b);
    ensure!(r.take(3)? == vec![b'A', b'P', 2], "bad magic");
    let nw = r.var()?;
    let mut witnesses = vec![];
    for _ in 0..nw {
        let t: Txid = bitcoin::consensus::encode::deserialize(&r.take(32)?)?;
        witnesses.push(t.to_string());
    }
    let nt = r.var()?;
    let mut psbts = vec![];
    for _ in 0..nt {
        let tx: Transaction = bitcoin::consensus::encode::deserialize(&r.bytes()?)?;
        let mut p = Psbt::from_unsigned_tx(tx)?;
        for i in 0..p.inputs.len() {
            match r.take(1)?[0] {
                0 => {}
                1 => p.inputs[i].tap_key_sig = Some(bitcoin::taproot::Signature::from_slice(&r.bytes()?)?),
                2 => {
                    let script = ScriptBuf::from_bytes(r.bytes()?);
                    let cb = ControlBlock::decode(&r.bytes()?)?;
                    let leaf = TapLeafHash::from_script(&script, LeafVersion::TapScript);
                    for pk in checksig_pks(&script) {
                        let sig = bitcoin::taproot::Signature::from_slice(&r.bytes()?)?;
                        p.inputs[i].tap_script_sigs.insert((pk, leaf), sig);
                    }
                    p.inputs[i].tap_scripts.insert(cb, (script, LeafVersion::TapScript));
                }
                t => anyhow::bail!("bad input tag {t}"),
            }
        }
        psbts.push(p.to_string());
    }
    Ok(ProofFile { version: 1, witnesses, psbts, recoveries: vec![], vtxos: vec![] })
}

fn compressed(path: &Path, tool: &str) -> u64 {
    std::process::Command::new(tool)
        .args(["-9", "-c", &path.display().to_string()])
        .output()
        .map(|o| o.stdout.len() as u64)
        .unwrap_or(0)
}

/// sizes of the proof in all encodings (+ writes the .bin next to it)
fn proof_sizes(json: &Path) -> anyhow::Result<serde_json::Value> {
    if !json.exists() {
        return Ok(serde_json::json!({"json": 0}));
    }
    let pf: ProofFile = serde_json::from_str(&std::fs::read_to_string(json)?)?;
    let bin = compact_encode(&pf)?;
    let binp = PathBuf::from(format!("{}.bin", json.display()));
    std::fs::write(&binp, &bin)?;
    // round trip must give the same txs + verifier-relevant data
    let back = compact_decode(&bin)?;
    ensure!(back.psbts.len() == pf.psbts.len(), "roundtrip");
    let raw_tx: usize = pf.psbts.iter().map(|b| bitcoin::consensus::encode::serialize(&Psbt::from_str(b).unwrap().unsigned_tx).len()).sum();
    Ok(serde_json::json!({
        "n_psbts": pf.psbts.len(),
        "json": file_len(json), "json_gz": compressed(json, "gzip"), "json_xz": compressed(json, "xz"),
        "bin": bin.len(), "bin_gz": compressed(&binp, "gzip"), "bin_xz": compressed(&binp, "xz"),
        "unsigned_tx_bytes": raw_tx,
    }))
}

fn cons_sizes(c: &Path) -> serde_json::Value {
    serde_json::json!({"raw": file_len(c), "gz": compressed(c, "gzip"), "xz": compressed(c, "xz")})
}

/// Cold/warm validation timing of `cons` (+ its proof) on fresh verifier wallets.
async fn time_validation(tag: &str, state: &Path, cons: &Path, ops: &str, rec_down: bool) -> anyhow::Result<serde_json::Value> {
    let p = proof_path(cons);
    let has_proof = p.exists() && !crate::proof::psbt_txids(&p)?.is_empty();
    let (v1, on1) = bl(|| rgb_wallet(&format!("ver-{tag}-a"), state))?;
    let (v2, on2) = bl(|| rgb_wallet(&format!("ver-{tag}-b"), state))?;
    let _down = if rec_down { Some(crate::proof::arkd_down(&mut |_k: &str, _v: String| {}, tag)?) } else { None };
    // cold, JSON proof
    let t = Instant::now();
    if has_proof {
        import(&v1, &on1, &p, ops)?;
    }
    let t_import = t.elapsed().as_secs_f64() * 1e3;
    bl(|| v1.validate_consignment_ark(on1.clone(), cons.to_path_buf(), true))?;
    let t_cold = t.elapsed().as_secs_f64() * 1e3;
    // warm: same wallet, everything again
    let t = Instant::now();
    if has_proof {
        import(&v1, &on1, &p, ops)?;
    }
    bl(|| v1.validate_consignment_ark(on1.clone(), cons.to_path_buf(), true))?;
    let t_warm = t.elapsed().as_secs_f64() * 1e3;
    // warm, consignment only (proof cached by the receiver)
    let t = Instant::now();
    bl(|| v1.validate_consignment_ark(on1.clone(), cons.to_path_buf(), true))?;
    let t_cons_only = t.elapsed().as_secs_f64() * 1e3;
    // cold, compact binary proof (decode + import + validate)
    let t = Instant::now();
    if has_proof {
        let bin = std::fs::read(format!("{}.bin", p.display()))?;
        let pf = compact_decode(&bin)?;
        let tmp = PathBuf::from(format!("{}.decoded.json", p.display()));
        std::fs::write(&tmp, serde_json::to_string(&pf)?)?;
        import(&v2, &on2, &tmp, ops)?;
    }
    bl(|| v2.validate_consignment_ark(on2.clone(), cons.to_path_buf(), true))?;
    let t_cold_bin = t.elapsed().as_secs_f64() * 1e3;
    Ok(serde_json::json!({"cold_ms": t_cold, "import_ms": t_import, "warm_ms": t_warm, "cons_only_ms": t_cons_only, "cold_bin_ms": t_cold_bin, "arkd_paused": rec_down}))
}

fn row(dir: &Path, phase: &str, v: serde_json::Value) -> anyhow::Result<()> {
    println!("ROW {phase} {v}");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{phase}.jsonl")))?;
    writeln!(f, "{v}")?;
    Ok(())
}

pub async fn run(dir: PathBuf, phase: &str, n: usize) -> anyhow::Result<()> {
    let state = dir.join(format!("{phase}{n}-users"));
    std::fs::create_dir_all(&state)?;
    let regtest = Arc::new(Regtest::new());
    let pause = pause_mode();
    let mut rec = |k: &str, v: String| println!("  >> {k} = {v}");
    match phase {
        "hops" => {
            let (a, _) = ark_client("a", regtest.clone(), &state).await?;
            let (b, _) = ark_client("b", regtest.clone(), &state).await?;
            let ops = operators(&a.server_info().await?);
            board("a", &a, "0.001").await?;
            let v0 = first_vtxo(&a).await?;
            let (mut ra, oa) = bl(|| rgb_wallet("a", &state))?;
            let (mut rb, ob) = bl(|| rgb_wallet("b", &state))?;
            let asset_id = onchain_genesis_jump_in(&regtest, &mut ra, &oa, v0, &state, &mut rec).await?;
            let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
            let mut vtxo = v0;
            let mut value = 100_000u64;
            let mut wits: Vec<Txid> = vec![];
            let mut prev_proof: Option<PathBuf> = None;
            let mut seen_by: [HashSet<String>; 2] = [HashSet::new(), HashSet::new()];
            for h in 1..=n {
                step(&format!("BENCH hop {h}/{n}"));
                let s_is_a = h % 2 == 1;
                let (sc, sr, rc) = if s_is_a { (&a, &ra, &b) } else { (&b, &rb, &a) };
                let (raddr, _) = rc.get_offchain_address().await?;
                let (chg, chg_v) = sc.get_offchain_address().await?;
                let send = value - 1_000;
                let t = Instant::now();
                let hop = rgb_hop(
                    &format!("hop{h}"), sc, sr, vtxo,
                    vec![SendReceiver::bitcoin(raddr, Amount::from_sat(send))],
                    &chg, chg_v.owner_pk(), cid, vec![(1, ISSUE_AMOUNT, 10_000 + h as u64)], 20_000 + h as u64,
                    state.join(format!("hop{h}.rgbc")),
                ).await?;
                let t_send = t.elapsed().as_secs_f64() * 1e3;
                vtxo = OutPoint { txid: hop.ark_txid, vout: 1 };
                value = send;
                wait_vtxo(rc, vtxo, "rcv").await?;
                wits.push(hop.checkpoint_txid);
                wits.push(hop.ark_txid);
                let t = Instant::now();
                let carried: Vec<PathBuf> = prev_proof.iter().cloned().collect();
                build_proof(sc, &regtest, &wits, &carried, &proof_path(&hop.consignment)).await?;
                let t_build = t.elapsed().as_secs_f64() * 1e3;
                let sizes = proof_sizes(&proof_path(&hop.consignment))?;
                // receiver cache: txs the receiver already verified in earlier hops
                let pf: ProofFile = serde_json::from_str(&std::fs::read_to_string(proof_path(&hop.consignment))?)?;
                let ri = if s_is_a { 1 } else { 0 };
                let new_psbts: Vec<&String> = pf.psbts.iter().filter(|p| !seen_by[ri].contains(*p)).collect();
                let incr_bytes: usize = new_psbts.iter().map(|p| p.len()).sum();
                let incr_bin: usize = {
                    let sub = ProofFile { version: 1, witnesses: pf.witnesses.clone(), psbts: new_psbts.iter().map(|s| s.to_string()).collect(), recoveries: vec![], vtxos: vec![] };
                    compact_encode(&sub)?.len()
                };
                for p in &pf.psbts {
                    seen_by[ri].insert(p.clone());
                }
                let timing = time_validation(&format!("hop{h}"), &state, &hop.consignment, &ops, pause).await?;
                // the real receiver accepts (proof import + accept)
                let t = Instant::now();
                {
                    let rr = if s_is_a { &mut rb } else { &mut ra };
                    let ro = if s_is_a { &ob } else { &oa };
                    import(rr, ro, &proof_path(&hop.consignment), &ops)?;
                    bl(|| rr.accept_transfer_consignment_offchain(ro.clone(), hop.consignment.clone(), vec![], hop.ark_txid.to_string(), 1, 10_000 + h as u64))?;
                }
                let t_accept = t.elapsed().as_secs_f64() * 1e3;
                row(&dir, "hops", serde_json::json!({
                    "hop": h, "consignment": cons_sizes(&hop.consignment), "proof": sizes,
                    "proof_incremental_psbt_b64_bytes": incr_bytes, "proof_incremental_bin": incr_bin, "n_new_psbts": new_psbts.len(),
                    "send_ms": t_send, "build_proof_ms": t_build, "accept_ms": t_accept, "validation": timing,
                }))?;
                prev_proof = Some(proof_path(&hop.consignment));
            }
        }
        "refresh" => {
            let (a, _) = ark_client("a", regtest.clone(), &state).await?;
            let ops = operators(&a.server_info().await?);
            board("a", &a, "0.001").await?;
            let v0 = first_vtxo(&a).await?;
            let (mut ra, oa) = bl(|| rgb_wallet("a", &state))?;
            let asset_id = onchain_genesis_jump_in(&regtest, &mut ra, &oa, v0, &state, &mut rec).await?;
            let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
            let mut v = v0;
            let mut forfeits: Vec<Txid> = vec![];
            let mut prev: Option<PathBuf> = None;
            for r in 1..=n {
                step(&format!("BENCH refresh {r}/{n}"));
                let (addr, _) = a.get_offchain_address().await?;
                let t = Instant::now();
                let (c, fc) = colored_batch(&format!("refresh{r}"), &a, &ra, cid, v, ISSUE_AMOUNT, BatchOp::Refresh(addr)).await?;
                mine_until(&regtest, &c).await?;
                let t_batch = t.elapsed().as_secs_f64() * 1e3;
                forfeits.push(fc.forfeit_tx.compute_txid());
                let cons = state.join(format!("refresh{r}.rgbc"));
                fc.transfer.save_file(&cons).map_err(|e| anyhow!("save: {e}"))?;
                let carried: Vec<PathBuf> = prev.iter().cloned().collect();
                build_proof(&a, &regtest, &forfeits, &carried, &proof_path(&cons)).await?;
                import(&ra, &oa, &proof_path(&cons), &ops)?;
                let _ = bl(|| ra.update_witnesses(oa.clone(), 0, vec![]));
                let sizes = proof_sizes(&proof_path(&cons))?;
                let timing = time_validation(&format!("refresh{r}"), &state, &cons, &ops, pause).await?;
                row(&dir, "refresh", serde_json::json!({"refreshes": r, "consignment": cons_sizes(&cons), "proof": sizes, "batch_ms": t_batch, "validation": timing}))?;
                v = fc.target;
                prev = Some(proof_path(&cons));
            }
        }
        "merge" | "onchain" => {
            // issuer I with k allocations; k on-chain payments I -> M; M merges them into one payment to R.
            // "onchain": a chain of n mined transfers P <-> Q (history depth).
            let k = n;
            let (mut ri, oi) = bl(|| rgb_wallet("issuer", &state))?;
            let addr = bl(|| ri.get_address())?;
            regtest::faucet(&Address::from_str(&addr)?.assume_checked(), "0.01")?;
            tokio::time::sleep(Duration::from_secs(3)).await;
            let slots = if phase == "merge" { k } else { 1 };
            bl(|| ri.create_utxos(oi.clone(), false, Some(slots as u8), Some(if phase == "merge" { 5_000 } else { 30_000 }), 2, false))?;
            regtest::mine(1)?;
            tokio::time::sleep(Duration::from_secs(3)).await;
            let asset = bl(|| ri.issue_asset_nia("USDX".into(), "USDt-like".into(), 0, vec![100; slots]))?;
            let cid = ContractId::from_str(&asset.asset_id).map_err(|e| anyhow!("{e}"))?;
            let alloc_utxos = |w: &mut RgbWallet, o: &Online| -> anyhow::Result<Vec<(OutPoint, u64, u64)>> {
                let us = bl(|| w.list_unspents(Some(o.clone()), false, false))?;
                Ok(us.iter()
                    .filter_map(|u| {
                        let amt: u64 = u.rgb_allocations.iter().filter(|a| a.asset_id.as_deref() == Some(asset.asset_id.as_str()) && a.settled).map(|a| match a.assignment { Assignment::Fungible(x) => x, _ => 0 }).sum();
                        (amt > 0).then(|| (OutPoint { txid: Txid::from_str(&u.utxo.outpoint.txid).unwrap(), vout: u.utxo.outpoint.vout }, u.utxo.btc_amount, amt))
                    })
                    .collect())
            };
            if phase == "merge" {
                let (mut rm, om) = bl(|| rgb_wallet("merger", &state))?;
                let (mut rr, or) = bl(|| rgb_wallet("recv", &state))?;
                let ins = alloc_utxos(&mut ri, &oi)?;
                ensure!(ins.len() == k, "issuer has {} allocations, want {k}", ins.len());
                let mut t_in = 0.0;
                for (j, (op, btc, amt)) in ins.iter().enumerate() {
                    let cons = state.join(format!("pay{j}.rgbc"));
                    let t = Instant::now();
                    onchain_pay(&mut ri, &oi, &mut rm, &om, &regtest, &[(*op, *btc)], cid, *amt, 3_000, &cons, 30_000 + j as u64).await?;
                    t_in += t.elapsed().as_secs_f64() * 1e3;
                }
                let ins = alloc_utxos(&mut rm, &om)?;
                ensure!(ins.len() == k, "merger has {} allocations, want {k}", ins.len());
                let total: u64 = ins.iter().map(|x| x.2).sum();
                let cons = state.join(format!("merge{k}.rgbc"));
                let t = Instant::now();
                onchain_pay(&mut rm, &om, &mut rr, &or, &regtest, &ins.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(), cid, total, 1_000, &cons, 40_000).await?;
                let t_merge = t.elapsed().as_secs_f64() * 1e3;
                let timing = time_validation(&format!("merge{k}"), &state, &cons, "00", false).await?;
                row(&dir, "merge", serde_json::json!({"k": k, "consignment": cons_sizes(&cons), "inputs_consignment_avg": cons_sizes(&state.join("pay0.rgbc")), "merge_send_and_accept_ms": t_merge, "inbound_payments_ms": t_in, "validation": timing}))?;
            } else {
                // a chain of fresh wallets W0 -> W1 -> ... (each receives once, then sends)
                let (mut cur, mut cur_o) = bl(|| rgb_wallet("w0", &state))?;
                let first = alloc_utxos(&mut ri, &oi)?;
                let cons0 = state.join("chain0.rgbc");
                onchain_pay(&mut ri, &oi, &mut cur, &cur_o, &regtest, &[(first[0].0, first[0].1)], cid, 100, 20_000, &cons0, 50_000).await?;
                for h in 1..=n {
                    step(&format!("BENCH on-chain hop {h}/{n}"));
                    let (mut nxt, nxt_o) = bl(|| rgb_wallet(&format!("w{h}"), &state))?;
                    let ins = alloc_utxos(&mut cur, &cur_o)?;
                    let cons = state.join(format!("chain{h}.rgbc"));
                    let btc = ins[0].1;
                    let t = Instant::now();
                    onchain_pay(&mut cur, &cur_o, &mut nxt, &nxt_o, &regtest, &[(ins[0].0, btc)], cid, 100, btc - 1_000, &cons, 50_000 + h as u64).await?;
                    let t_pay = t.elapsed().as_secs_f64() * 1e3;
                    let timing = time_validation(&format!("chain{h}"), &state, &cons, "00", false).await?;
                    row(&dir, "onchain", serde_json::json!({"hop": h, "consignment": cons_sizes(&cons), "pay_and_accept_ms": t_pay, "validation": timing}))?;
                    cur = nxt;
                    cur_o = nxt_o;
                }
            }
        }
        _ => anyhow::bail!("unknown phase {phase}"),
    }
    println!("\nBENCH {phase} DONE");
    Ok(())
}

/// On-chain RGB payment of `amount` (all the inputs' allocations) from `s` to a fresh witness
/// receive of `r`, mined, accepted by `r` until settled.
#[allow(clippy::too_many_arguments)]
async fn onchain_pay(
    s: &mut RgbWallet,
    so: &Online,
    r: &mut RgbWallet,
    ro: &Online,
    regtest: &Regtest,
    ins: &[(OutPoint, u64)],
    cid: ContractId,
    amount: u64,
    out_sat: u64,
    cons: &Path,
    blinding: u64,
) -> anyhow::Result<Txid> {
    let recv = bl(|| r.witness_receive(None, Assignment::Fungible(amount), now() + 86_400, vec![], 1))?;
    let to = rgb_lib::utils::script_buf_from_recipient_id(recv.recipient_id.clone())?.ok_or_else(|| anyhow!("not witness"))?;
    let mut inputs = vec![];
    for (op, _) in ins {
        let mut got = None;
        for _ in 0..10 {
            let _ = bl(|| s.list_unspents(Some(so.clone()), false, false));
            let _ = bl(|| s.sync(so.clone(), rgb_lib::wallet::SyncOptions { keychain: rgb_lib::wallet::SyncKeychain::Colored, strategy: rgb_lib::wallet::SyncStrategy::FullSync }));
            if let Ok(i) = bl(|| s.psbt_input_for_utxo(Outpoint { txid: op.txid.to_string(), vout: op.vout })) {
                got = Some(i);
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        inputs.push(got.ok_or_else(|| anyhow!("no psbt input for {op}"))?);
    }
    let total: u64 = ins.iter().map(|x| x.1).sum();
    let fee = 200 + 70 * ins.len() as u64;
    let change = Address::from_str(&bl(|| s.get_address())?)?.assume_checked();
    let mut output = vec![bare_opret(Amount::ZERO), TxOut { value: Amount::from_sat(out_sat), script_pubkey: to }];
    let chg = total.checked_sub(out_sat + fee).ok_or_else(|| anyhow!("not enough btc"))?;
    if chg >= 330 {
        output.push(TxOut { value: Amount::from_sat(chg), script_pubkey: change.script_pubkey() });
    }
    let unsigned = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: ins.iter().map(|(op, _)| bitcoin::TxIn { previous_output: *op, sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME, ..Default::default() }).collect(),
        output,
    };
    let mut psbt = Psbt::from_unsigned_tx(unsigned)?;
    psbt.inputs = inputs;
    let t = bl(|| s.color_psbt_and_consume_custom(&mut psbt, cid, vec![(SealTarget::WitnessVout(1), amount, blinding)]))?;
    t.save_file(cons).map_err(|e| anyhow!("save: {e}"))?;
    let signed = Psbt::from_str(&bl(|| s.sign_psbt(psbt.to_string(), None))?)?;
    let tx = signed.extract_tx().map_err(|e| anyhow!("extract: {e}"))?;
    let txid = regtest.send_raw(&tx)?;
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut ok = false;
    for _ in 0..10 {
        if bl(|| r.provide_out_of_band_consignment(ro.clone(), cons.display().to_string(), vec![])).is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    ensure!(ok, "receiver could not take the consignment");
    for _ in 0..8 {
        regtest::mine(1)?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        bl(|| r.refresh(ro.clone(), None, vec![], false))?;
        let _ = bl(|| s.refresh(so.clone(), None, vec![], false));
        let b = bl(|| r.get_asset_balance(cid.to_string()))?;
        if b.settled >= amount {
            return Ok(txid);
        }
    }
    anyhow::bail!("payment {txid} never settled")
}

#[allow(dead_code)]
fn _unused(_: &ArkClient) {}

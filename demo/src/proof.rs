//! Self-contained Ark proofs (sidecar `<consignment>.arkproof.json`).
//!
//! Sender side (uses arkd): collect the PSBT of every unmined RGB witness and of every unmined
//! ancestor down to a mined tx (`GetVirtualTxs`), carrying the proofs received earlier.
//! Receiver side (no arkd): rgb-lib `import_ark_proof` verifies the bundle against the chain only.
//! `offline_mode()` is on with `--offline-proof`; then arkd is paused (`docker pause arkd`) while
//! every receiver validates.
use crate::{ArkClient, bl, regtest};
use anyhow::{anyhow, ensure};
use bitcoin::{Psbt, Txid};
use regtest::Regtest;
use rgb_lib::wallet::{Online, Wallet as RgbWallet};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

pub(crate) fn offline_mode() -> bool {
    std::env::args().any(|a| a == "--offline-proof")
}

pub(crate) fn proof_path(consignment: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", consignment.display(), rgb_lib::ark_proof::ARK_PROOF_SUFFIX))
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(crate) struct ProofFile {
    pub version: u8,
    pub witnesses: Vec<String>,
    pub psbts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recoveries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vtxos: Vec<String>,
}

/// Declare `fid` (a witness of the proof) as a sweep-recovery forfeit.
pub(crate) fn mark_recovery(path: &Path, fid: Txid) -> anyhow::Result<()> {
    let mut f = load(path)?;
    ensure!(f.witnesses.contains(&fid.to_string()), "{fid} is not a witness of {}", path.display());
    if !f.recoveries.contains(&fid.to_string()) {
        f.recoveries.push(fid.to_string());
    }
    std::fs::write(path, serde_json::to_string(&f)?)?;
    Ok(())
}

/// Bundle the branch of each VTXO in `vtxos` (unmined ancestors from arkd) and list it,
/// so the receiver can track its liveness from the chain.
pub(crate) async fn add_vtxos(client: &ArkClient, regtest: &Regtest, path: &Path, vtxos: &[bitcoin::OutPoint]) -> anyhow::Result<()> {
    let mut f = if path.exists() { load(path)? } else { ProofFile { version: 1, witnesses: vec![], psbts: vec![], recoveries: vec![], vtxos: vec![] } };
    let mut have: HashSet<Txid> = f.psbts.iter().filter_map(|b| Psbt::from_str(b).ok()).map(|p| p.unsigned_tx.compute_txid()).collect();
    let mut queue: VecDeque<Txid> = vtxos.iter().map(|o| o.txid).filter(|t| regtest.tx_confirmations(t).is_none()).collect();
    while let Some(t) = queue.pop_front() {
        if have.contains(&t) {
            continue;
        }
        let r = client.network_client().get_virtual_txs(vec![t.to_string()], None, None).await.map_err(|e| anyhow!("GetVirtualTxs({t}): {e:?}"))?;
        let p = r.txs.into_iter().find(|p| p.unsigned_tx.compute_txid() == t).ok_or_else(|| anyhow!("{t} not a virtual tx"))?;
        for i in &p.unsigned_tx.input {
            if regtest.tx_confirmations(&i.previous_output.txid).is_none() {
                queue.push_back(i.previous_output.txid);
            }
        }
        have.insert(t);
        f.psbts.push(p.to_string());
    }
    for o in vtxos {
        if !f.vtxos.contains(&o.to_string()) {
            f.vtxos.push(o.to_string());
        }
    }
    std::fs::write(path, serde_json::to_string(&f)?)?;
    Ok(())
}

pub(crate) struct ProofStats {
    pub bytes: u64,
    pub n_psbts: usize,
    pub n_witnesses: usize,
}

fn load(path: &Path) -> anyhow::Result<ProofFile> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// Build the proof for `witnesses` (unmined ones are kept), reusing PSBTs of `carried` proofs.
pub(crate) async fn build_proof(
    client: &ArkClient,
    regtest: &Regtest,
    witnesses: &[Txid],
    carried: &[PathBuf],
    out: &Path,
) -> anyhow::Result<ProofStats> {
    let mut known: HashMap<Txid, Psbt> = HashMap::new();
    let mut carried_recoveries: Vec<String> = vec![];
    for c in carried {
        carried_recoveries.extend(load(c)?.recoveries);
        for b in load(c)?.psbts {
            let p = Psbt::from_str(&b)?;
            known.insert(p.unsigned_tx.compute_txid(), p);
        }
    }
    let unmined: Vec<Txid> = witnesses.iter().copied().filter(|t| regtest.tx_confirmations(t).is_none()).collect();
    let mut bundle: HashMap<Txid, Psbt> = HashMap::new();
    let mut queue: VecDeque<Txid> = unmined.iter().copied().collect();
    let mut seen = HashSet::new();
    while let Some(t) = queue.pop_front() {
        if !seen.insert(t) {
            continue;
        }
        let p = match known.get(&t) {
            Some(p) => p.clone(),
            None => {
                // a commitment tx is not a virtual tx: arkd returns it as raw hex, not a PSBT
                // a witness finalized a moment ago may not be indexed yet: retry those briefly
                let mut found = None;
                for _ in 0..if unmined.contains(&t) { 10 } else { 1 } {
                    found = match client.network_client().get_virtual_txs(vec![t.to_string()], None, None).await {
                        Ok(r) => r.txs.into_iter().find(|p| p.unsigned_tx.compute_txid() == t),
                        Err(e) => {
                            println!("[proof] GetVirtualTxs({t}) not a virtual tx: {e:?}");
                            None
                        }
                    };
                    if found.is_some() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                match found {
                    Some(p) => p,
                    None => {
                        println!("[proof] {t} is neither mined nor a known virtual tx: left out");
                        continue;
                    }
                }
            }
        };
        for i in &p.unsigned_tx.input {
            let prev = i.previous_output.txid;
            if regtest.tx_confirmations(&prev).is_none() {
                queue.push_back(prev);
            }
        }
        bundle.insert(t, p);
    }
    let witnesses: Vec<String> = unmined.iter().map(|t| t.to_string()).collect();
    carried_recoveries.retain(|r| witnesses.contains(r));
    carried_recoveries.dedup();
    let file = ProofFile {
        version: 1,
        witnesses,
        psbts: bundle.values().map(|p| p.to_string()).collect(),
        recoveries: carried_recoveries,
        vtxos: vec![],
    };
    std::fs::write(out, serde_json::to_string(&file)?)?;
    let bytes = std::fs::metadata(out)?.len();
    println!("[proof] {} : {} witnesses, {} PSBTs, {} bytes", out.display(), file.witnesses.len(), file.psbts.len(), bytes);
    Ok(ProofStats { bytes, n_psbts: file.psbts.len(), n_witnesses: file.witnesses.len() })
}

fn arkd_reachable() -> bool {
    std::process::Command::new("curl")
        .args(["-s", "--max-time", "3", "-o", "/dev/null", "-w", "%{http_code}", "http://localhost:7070/v1/info"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "200")
        .unwrap_or(false)
}

/// arkd paused for as long as this guard lives (unpaused on drop, also on errors).
pub(crate) struct ArkdPaused;

impl Drop for ArkdPaused {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker").args(["unpause", "arkd"]).status();
        for _ in 0..20 {
            if arkd_reachable() {
                println!("arkd unpaused");
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        println!("WARNING: arkd not reachable after unpause");
    }
}

/// Pause arkd (indexer unreachable) and prove it.
pub(crate) fn arkd_down(rec: &mut dyn FnMut(&str, String), label: &str) -> anyhow::Result<ArkdPaused> {
    let s = std::process::Command::new("docker").args(["pause", "arkd"]).status()?;
    ensure!(s.success(), "docker pause arkd failed");
    let guard = ArkdPaused;
    let reachable = arkd_reachable();
    println!("[{label}] arkd paused; /v1/info reachable = {reachable}");
    rec(&format!("{label}_arkd_reachable_during_validation"), reachable.to_string());
    ensure!(!reachable, "arkd still reachable");
    Ok(guard)
}

/// Import a proof into `rgb` (offline); returns the registered witnesses.
pub(crate) fn import(rgb: &RgbWallet, online: &Online, path: &Path, operators: &str) -> anyhow::Result<Vec<String>> {
    bl(|| rgb.import_ark_proof(online.clone(), path.to_path_buf(), operators.to_string())).map_err(|e| anyhow!("{e}"))
}

/// Negative tests on mutated copies of a valid proof; every import must fail.
pub(crate) fn negatives(
    who: &str,
    rgb: &RgbWallet,
    online: &Online,
    proof: &Path,
    operators: &str,
    rec: &mut dyn FnMut(&str, String),
) -> anyhow::Result<bool> {
    let base = load(proof)?;
    let psbts: Vec<Psbt> = base.psbts.iter().map(|b| Psbt::from_str(b)).collect::<Result<_, _>>()?;
    let wit: HashSet<String> = base.witnesses.iter().cloned().collect();
    let txid = |p: &Psbt| p.unsigned_tx.compute_txid().to_string();
    let mut cases: Vec<(String, ProofFile)> = vec![];
    // 1. missing branch tx: drop the first non-witness PSBT
    if let Some(i) = psbts.iter().position(|p| !wit.contains(&txid(p))) {
        let mut f = base.clone();
        f.psbts.remove(i);
        cases.push((format!("missing_branch_tx_{}", &txid(&psbts[i])[..8]), f));
    }
    // 2. tampered branch: corrupt a key-path signature of a tree node
    if let Some(i) = psbts.iter().position(|p| !wit.contains(&txid(p)) && p.inputs.iter().any(|x| x.tap_key_sig.is_some())) {
        let mut p = psbts[i].clone();
        let mut sig = p.inputs[0].tap_key_sig.unwrap();
        let mut b = sig.signature.serialize();
        b[10] ^= 1;
        sig.signature = bitcoin::secp256k1::schnorr::Signature::from_slice(&b)?;
        p.inputs[0].tap_key_sig = Some(sig);
        let mut f = base.clone();
        f.psbts[i] = p.to_string();
        cases.push(("tampered_branch_signature".into(), f));
    }
    // 3. tampered branch: change an output value of a non-witness tx (txid changes)
    if let Some(i) = psbts.iter().position(|p| !wit.contains(&txid(p)) && p.unsigned_tx.output.iter().any(|o| o.value.to_sat() > 1000)) {
        let mut p = psbts[i].clone();
        let o = p.unsigned_tx.output.iter_mut().find(|o| o.value.to_sat() > 1000).unwrap();
        o.value -= bitcoin::Amount::from_sat(1);
        let mut f = base.clone();
        f.psbts[i] = p.to_string();
        cases.push(("tampered_branch_output".into(), f));
    }
    // forfeit cases: a witness with 2 inputs (connector, VTXO)
    if let Some(fi) = psbts.iter().position(|p| wit.contains(&txid(p)) && p.unsigned_tx.input.len() == 2) {
        let conn = psbts[fi].unsigned_tx.input[0].previous_output.txid;
        // 4. forfeit not linked to the connector: connector branch removed
        if let Some(ci) = psbts.iter().position(|p| p.unsigned_tx.compute_txid() == conn) {
            let mut f = base.clone();
            f.psbts.remove(ci);
            cases.push(("forfeit_connector_missing".into(), f));
            // 4b. fabricated connector (different tx in its place, so the forfeit links to nothing)
            let mut p = psbts[ci].clone();
            p.unsigned_tx.lock_time = bitcoin::absolute::LockTime::from_consensus(1);
            let mut f = base.clone();
            f.psbts[ci] = p.to_string();
            cases.push(("forfeit_connector_fabricated".into(), f));
        }
        // 5. forfeit without the operator co-signature (user signature only)
        let mut p = psbts[fi].clone();
        let ops: Vec<bitcoin::XOnlyPublicKey> = operators
            .split(',')
            .filter_map(|k| bitcoin::PublicKey::from_str(k).ok().map(|pk| pk.inner.x_only_public_key().0))
            .collect();
        p.inputs[1].tap_script_sigs.retain(|(pk, _), _| !ops.contains(pk));
        let mut f = base.clone();
        f.psbts[fi] = p.to_string();
        cases.push(("forfeit_without_operator_sig".into(), f));
        // 6. forfeit with its OP_RETURN removed from the claimed layout is a different txid:
        //    covered by the arkd negatives; here: witness listed but its PSBT missing
        let mut f = base.clone();
        f.psbts.remove(fi);
        cases.push(("forfeit_psbt_missing".into(), f));
    }
    let mut all = true;
    for (name, f) in cases {
        let path = proof.with_extension(format!("neg-{name}.json"));
        std::fs::write(&path, serde_json::to_string(&f)?)?;
        let r = import(rgb, online, &path, operators);
        let rejected = r.is_err();
        println!("[{who}] NEG proof {name}: {}", match &r { Ok(v) => format!("ACCEPTED {v:?}"), Err(e) => format!("rejected: {e}") });
        rec(&format!("neg_proof_{who}_{name}"), if rejected { format!("rejected: {}", r.unwrap_err()).chars().take(300).collect() } else { "ACCEPTED".into() });
        all &= rejected;
    }
    // wrong pinned operator key
    let r = import(rgb, online, proof, "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5");
    println!("[{who}] NEG proof wrong_operator_key: {:?}", r.as_ref().map_err(|e| e.to_string()));
    rec(&format!("neg_proof_{who}_wrong_operator_key"), r.is_err().to_string());
    all &= r.is_err();
    Ok(all)
}

/// Negative test: a never-mined tx spending an on-chain P2TR output by key path, with a well-formed
/// opret and no operator signature. Its owner could sign any number of them, so it must be rejected.
pub(crate) fn negative_key_path_witness(
    who: &str,
    rgb: &RgbWallet,
    online: &Online,
    regtest: &Regtest,
    proof: &Path,
    operators: &str,
    rec: &mut dyn FnMut(&str, String),
) -> anyhow::Result<bool> {
    use bitcoin::hashes::Hash;
    use bitcoin::key::{Keypair, TapTweak};
    use bitcoin::secp256k1::{Message, Secp256k1, rand::thread_rng};
    use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
    use bitcoin::{Address, Amount, Network, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness};

    let secp = Secp256k1::new();
    let kp = Keypair::new(&secp, &mut thread_rng());
    let addr = Address::p2tr(&secp, kp.x_only_public_key().0, None, Network::Regtest);
    regtest::faucet(&addr, "0.0001")?;
    let mut utxo = None;
    for _ in 0..30 {
        utxo = regtest.find_outpoints_blocking(&addr).into_iter().find(|u| u.confirmations > 0);
        if utxo.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let utxo = utxo.ok_or_else(|| anyhow!("faucet output to {addr} not confirmed"))?;
    let tx = Transaction {
        version: bitcoin::transaction::Version::non_standard(3),
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn { previous_output: utxo.outpoint, script_sig: ScriptBuf::new(), sequence: Sequence::MAX, witness: Witness::new() }],
        output: vec![TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return([0xaa; 32]) }],
    };
    let prevout = TxOut { value: utxo.amount, script_pubkey: addr.script_pubkey() };
    let h = SighashCache::new(&tx).taproot_key_spend_signature_hash(0, &Prevouts::All(&[prevout]), TapSighashType::Default)?;
    let signature = secp.sign_schnorr_no_aux_rand(&Message::from_digest(h.to_byte_array()), &kp.tap_tweak(&secp, None).to_keypair());
    let mut psbt = Psbt::from_unsigned_tx(tx.clone())?;
    psbt.inputs[0].tap_key_sig = Some(bitcoin::taproot::Signature { signature, sighash_type: TapSighashType::Default });
    let f = ProofFile { version: 1, witnesses: vec![tx.compute_txid().to_string()], psbts: vec![psbt.to_string()], recoveries: vec![], vtxos: vec![] };
    let path = proof.with_extension("neg-key_path_witness.json");
    std::fs::write(&path, serde_json::to_string(&f)?)?;
    // the indexer behind the resolver can lag behind the faucet block
    let mut r = import(rgb, online, &path, operators);
    for _ in 0..10 {
        if !matches!(&r, Err(e) if e.to_string().contains("neither in the proof nor mined")) {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
        r = import(rgb, online, &path, operators);
    }
    println!("[{who}] NEG proof key_path_witness: {}", match &r { Ok(v) => format!("ACCEPTED {v:?}"), Err(e) => format!("rejected: {e}") });
    let ok = matches!(&r, Err(e) if e.to_string().contains("no input co-signed by the operator"));
    rec(&format!("neg_proof_{who}_key_path_witness"), match r { Ok(_) => "ACCEPTED".into(), Err(e) => format!("rejected: {e}").chars().take(300).collect() });
    Ok(ok)
}

pub(crate) fn operators(info: &ark_core::server::Info) -> String {
    format!("{},{}", info.signer_pk, info.forfeit_pk)
}

pub(crate) fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

pub(crate) fn psbt_txids(p: &Path) -> anyhow::Result<Vec<String>> {
    Ok(load(p)?.psbts.iter().filter_map(|b| Psbt::from_str(b).ok()).map(|p| p.unsigned_tx.compute_txid().to_string()).collect())
}

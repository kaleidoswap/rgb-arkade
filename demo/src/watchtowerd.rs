//! Standalone exit-watchtower daemon (`ark-rgb-demo watchtowerd <dir>`).
//!
//! * Own state dir: `<dir>/registrations/*.json` (written by wallets before going offline),
//!   `<dir>/status/<id>.json` (persisted progress, so a restart resumes), `<dir>/daemon.log`.
//! * Holds NO keys at all. Each registration carries the user's fully signed exit branch plus, per
//!   branch tx, a *fee ladder*: CPFP children presigned by the user (own fee UTXO, change back to
//!   the user) at increasing feerates. The daemon only picks a rung and broadcasts
//!   `[branch tx, child]` with `submitpackage`; on rejection it climbs to the next rung.
//! * Decision at `expires_at - threshold_s`: refreshed (indexer `spent_by`) -> nothing; swept ->
//!   too late; otherwise exit. If arkd's indexer is unreachable it fails safe: it exits once the
//!   registration's own `expires_at` is within the threshold (worst case: an unneeded exit).
use crate::regtest::Regtest;
use anyhow::anyhow;
use ark_core::server::GetVtxosRequest;
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::{
    Amount, CompressedPublicKey, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute, transaction,
};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

pub(crate) const P2A: &str = "51024e73";
pub(crate) const EST_CHILD_VB: u64 = 153;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(crate) struct Rung {
    pub rate: u64,
    pub fee: u64,
    pub child: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(crate) struct Registration {
    pub id: String,
    pub vtxo: String,
    pub expires_at: i64,
    pub threshold_s: i64,
    pub branch: Vec<String>,
    /// one ladder per branch tx (same index), rungs sorted by rate
    pub ladders: Vec<Vec<Rung>>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub(crate) struct Status {
    pub state: String,
    pub events: Vec<String>,
    /// next rung to try, per branch level
    pub rung: Vec<usize>,
    pub broadcast: Vec<String>,
}

/// P2WPKH key used by the *user* to presign fee children (never given to the watchtower).
pub(crate) struct FeeKey {
    pub secp: Secp256k1<bitcoin::secp256k1::All>,
    pub sk: SecretKey,
    pub pk: CompressedPublicKey,
    pub spk: ScriptBuf,
}

impl FeeKey {
    pub fn new(sk: SecretKey) -> anyhow::Result<Self> {
        let secp = Secp256k1::new();
        let pk = CompressedPublicKey::from_private_key(&secp, &bitcoin::PrivateKey::new(sk, bitcoin::Network::Regtest))
            .map_err(|e| anyhow!("{e}"))?;
        let spk = ScriptBuf::new_p2wpkh(&pk.wpubkey_hash());
        Ok(Self { secp, sk, pk, spk })
    }

    /// v3 CPFP child: [parent P2A anchor, fee UTXO] -> change to the fee key. Fee is absolute.
    pub fn cpfp(&self, parent: &Transaction, utxo: (OutPoint, Amount), fee: u64) -> anyhow::Result<Transaction> {
        let anchor = parent
            .output
            .iter()
            .position(|o| o.script_pubkey.to_hex_string() == P2A)
            .ok_or_else(|| anyhow!("no P2A anchor in {}", parent.compute_txid()))?;
        let mut tx = Transaction {
            version: transaction::Version(3),
            lock_time: absolute::LockTime::ZERO,
            input: vec![
                TxIn { previous_output: OutPoint { txid: parent.compute_txid(), vout: anchor as u32 }, sequence: Sequence::ENABLE_RBF_NO_LOCKTIME, ..Default::default() },
                TxIn { previous_output: utxo.0, sequence: Sequence::ENABLE_RBF_NO_LOCKTIME, ..Default::default() },
            ],
            output: vec![TxOut { value: utxo.1 + parent.output[anchor].value - Amount::from_sat(fee), script_pubkey: self.spk.clone() }],
        };
        let h = SighashCache::new(&tx).p2wpkh_signature_hash(1, &self.spk, utxo.1, EcdsaSighashType::All)?;
        use bitcoin::hashes::Hash;
        let sig = self.secp.sign_ecdsa(&Message::from_digest(h.to_byte_array()), &self.sk);
        tx.input[1].witness = Witness::p2wpkh(&bitcoin::ecdsa::Signature::sighash_all(sig), &self.pk.0);
        Ok(tx)
    }

    /// Fee ladder for one branch tx.
    pub fn ladder(&self, parent: &Transaction, utxo: (OutPoint, Amount), rates: &[u64]) -> anyhow::Result<Vec<Rung>> {
        rates
            .iter()
            .map(|r| {
                let fee = r * (parent.vsize() as u64 + EST_CHILD_VB);
                let child = self.cpfp(parent, utxo, fee)?;
                Ok(Rung { rate: *r, fee, child: bitcoin::consensus::encode::serialize_hex(&child) })
            })
            .collect()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn tx_from_hex(h: &str) -> anyhow::Result<Transaction> {
    Ok(bitcoin::consensus::encode::deserialize_hex(h)?)
}

struct Daemon {
    dir: PathBuf,
    regtest: Regtest,
    indexer: Option<ark_grpc::Client>,
    target_rate: u64,
}

impl Daemon {
    fn log(&self, s: &str) {
        let line = format!("{} {s}", chrono_now());
        println!("{line}");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("daemon.log")) {
            let _ = writeln!(f, "{line}");
        }
    }

    async fn indexer_status(&mut self, vtxo: OutPoint) -> Option<ark_core::server::VirtualTxOutPoint> {
        if self.indexer.is_none() {
            let mut c = ark_grpc::Client::new("http://localhost:7070".to_string());
            match tokio::time::timeout(Duration::from_secs(5), c.connect()).await {
                Ok(Ok(())) => self.indexer = Some(c),
                _ => return None,
            }
        }
        let c = self.indexer.as_ref()?;
        match tokio::time::timeout(Duration::from_secs(5), c.list_vtxos(GetVtxosRequest::new_for_outpoints(&[vtxo]))).await {
            Ok(Ok(r)) => r.vtxos.into_iter().next(),
            _ => {
                self.indexer = None; // reconnect next time
                None
            }
        }
    }

    fn confirmed(&self, t: &Txid) -> bool {
        self.regtest.tx_confirmations(t).is_some()
    }

    fn in_mempool(&self, t: &Txid) -> bool {
        self.regtest.rpc("getmempoolentry", serde_json::json!([t.to_string()])).is_ok()
    }

    async fn step(&mut self, reg: &Registration, st: &mut Status) -> anyhow::Result<()> {
        let vtxo = OutPoint::from_str(&reg.vtxo)?;
        let branch: Vec<Transaction> = reg.branch.iter().map(|h| tx_from_hex(h)).collect::<Result<_, _>>()?;
        if st.rung.is_empty() {
            st.rung = reg.ladders.iter().map(|l| l.iter().position(|r| r.rate >= self.target_rate).unwrap_or(0)).collect();
        }
        match st.state.as_str() {
            "" | "watching" => {
                st.state = "watching".into();
                let info = self.indexer_status(vtxo).await;
                let expires = info.as_ref().map(|v| v.expires_at).unwrap_or(reg.expires_at);
                let left = expires - now();
                if let Some(v) = &info {
                    if v.is_spent && !v.is_unrolled {
                        let m = format!("{}: refreshed/spent off-chain (spent_by {:?}) -> nothing to do", reg.id, v.spent_by);
                        self.log(&m);
                        st.events.push(m);
                        st.state = "done-refreshed".into();
                        return Ok(());
                    }
                    if v.is_swept {
                        let m = format!("{}: already swept -> too late", reg.id);
                        self.log(&m);
                        st.events.push(m);
                        st.state = "too-late".into();
                        return Ok(());
                    }
                }
                if left <= reg.threshold_s {
                    let m = if info.is_some() {
                        format!("{}: {left}s to expiry, not refreshed -> exit", reg.id)
                    } else {
                        format!("{}: {left}s to expiry, indexer UNREACHABLE -> fail-safe exit", reg.id)
                    };
                    self.log(&m);
                    st.events.push(m);
                    st.state = "exiting".into();
                }
            }
            "exiting" => {
                let Some(i) = branch.iter().position(|t| !self.confirmed(&t.compute_txid())) else {
                    let m = format!("{}: whole branch confirmed", reg.id);
                    self.log(&m);
                    st.events.push(m);
                    st.state = "done-onchain".into();
                    return Ok(());
                };
                let parent = &branch[i];
                let pid = parent.compute_txid();
                if self.in_mempool(&pid) {
                    return Ok(());
                }
                let ladder = &reg.ladders[i];
                let k = st.rung[i].min(ladder.len() - 1);
                let rung = &ladder[k];
                let child = tx_from_hex(&rung.child)?;
                let hexes = vec![reg.branch[i].clone(), rung.child.clone()];
                let r = self.regtest.rpc("submitpackage", serde_json::json!([hexes, 0.1, 0.0001]));
                match r {
                    Ok(v) if v["package_msg"].as_str() == Some("success") => {
                        let m = format!(
                            "{}: broadcast level {i} {pid} (vsize {}) + presigned child {} (vsize {}) rung {} sat/vB fee {} sat",
                            reg.id, parent.vsize(), child.compute_txid(), child.vsize(), rung.rate, rung.fee
                        );
                        self.log(&m);
                        st.events.push(m.clone());
                        st.broadcast.push(m);
                    }
                    other => {
                        let m = format!("{}: level {i} rung {} rejected: {}", reg.id, rung.rate, format!("{other:?}").chars().take(300).collect::<String>());
                        self.log(&m);
                        st.events.push(m);
                        if k + 1 < ladder.len() {
                            st.rung[i] = k + 1;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn chrono_now() -> String {
    String::from_utf8(std::process::Command::new("date").arg("+%H:%M:%S").output().map(|o| o.stdout).unwrap_or_default())
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub(crate) fn registration_dir(dir: &Path) -> PathBuf {
    dir.join("registrations")
}

pub(crate) fn status_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("status").join(format!("{id}.json"))
}

pub(crate) fn read_status(dir: &Path, id: &str) -> Option<Status> {
    serde_json::from_str(&std::fs::read_to_string(status_path(dir, id)).ok()?).ok()
}

pub async fn run_daemon(dir: PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(registration_dir(&dir))?;
    std::fs::create_dir_all(dir.join("status"))?;
    let mut d = Daemon { dir: dir.clone(), regtest: Regtest::new(), indexer: None, target_rate: 2 };
    d.log(&format!("watchtowerd started (pid {}), dir {}", std::process::id(), dir.display()));
    loop {
        let mut entries: Vec<_> = std::fs::read_dir(registration_dir(&dir))?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let reg: Registration = match std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok()) {
                Some(r) => r,
                None => continue,
            };
            let mut st = read_status(&dir, &reg.id).unwrap_or_default();
            if st.state.starts_with("done") || st.state == "too-late" {
                continue;
            }
            if let Err(e) = d.step(&reg, &mut st).await {
                d.log(&format!("{}: step error {e:#}", reg.id));
            }
            std::fs::write(status_path(&dir, &reg.id), serde_json::to_string_pretty(&st)?)?;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

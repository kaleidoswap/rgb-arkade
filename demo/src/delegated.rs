//! RGB-aware renewal delegate daemon (`ark-rgb-demo delegated <dir>`), operator-risk option O2.
//!
//! * `<dir>/delegate-sk.hex`: the delegate key D (the only signing key it has; no owner key).
//!   `<dir>/fee-sk.hex`: its own P2WPKH key for the watchtower fee ladders it pays for.
//! * `<dir>/jobs/<id>.json`: one per owner, written by the owner before going offline (VTXOs,
//!   templates, consignment and proof of the asset, watchtower dirs, timing). The daemon keeps its
//!   progress in the same file, so a restart resumes.
//! * Output per job in `<dir>/out/<id>/`: `renewal-<k>.rgbc` + proof, `handoff.json` (what the
//!   owner needs on return), `metrics.jsonl` (proof size and verification time per renewal).
//!
//! Per job: accept (import the owner's proof and consignment into the delegate's own RGB wallet,
//! hand the exit branch to the watchtowers), then renew the colored VTXO as soon as its delegate
//! leaf opens (`T`, see [`crate::delegate`]). If the operator does not cosign (any error) it
//! retries; the watchtowers exit on their own before expiry. A colored VTXO that is on-chain is
//! parked: it is never put back into a batch (#961 guard).
use crate::delegate::{self, Handoff, Kind, Leaf, Template};
use crate::proof::{add_vtxos, build_proof, import, operators};
use crate::regtest::Regtest;
use crate::{ArkClient, bl, now};
use anyhow::{anyhow, ensure};
use ark_core::send::SendReceiver;
use bitcoin::key::Keypair;
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Amount, OutPoint, Txid};
use rgb_lib::wallet::{Online, Wallet as RgbWallet};
use rgb_lib::{ContractId, FileContent};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub(crate) struct Job {
    pub id: String,
    /// arkd endpoint the delegate uses for this owner
    pub arkd_url: String,
    pub asset_id: String,
    pub units: u64,
    pub colored: String,
    pub colored_template: Option<Template>,
    pub colored_sat: u64,
    pub colored_expires_at: i64,
    pub spare: String,
    pub spare_template: Option<Template>,
    pub spare_sat: u64,
    /// latest consignment and proof of the asset (owner's at first, then the delegate's)
    pub consignment: String,
    pub proof: String,
    /// unmined RGB witnesses of the history
    pub witnesses: Vec<String>,
    pub towers: Vec<PathBuf>,
    /// renew once `T` has passed by this margin
    pub t_margin_s: i64,
    /// the delegate leaf opens this long before expiry
    pub window_s: u64,
    pub expiry_s: u64,
    pub tower_threshold_s: i64,
    pub t_offline: u64,
    // progress
    pub state: String,
    pub renewals: u32,
    pub pending_f: Option<(String, Template, i64)>,
    pub leftover: Option<String>,
    pub templates: Vec<Template>,
    pub colored_history: Vec<String>,
    pub failures: u32,
    pub events: Vec<String>,
    /// threshold delegate: the other delegates' co-signer addresses and this delegate's index
    #[serde(default)]
    pub cosigners: Option<crate::cosign::Cosigners>,
    #[serde(default)]
    pub me: usize,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(crate) struct Metric {
    pub renewal: u32,
    pub at: u64,
    pub days_offline: f64,
    pub checkpoint: String,
    pub ark_tx: String,
    pub spare_batch: String,
    pub leftover_batch: String,
    pub new_vtxo: String,
    pub expires_in_s: i64,
    pub witnesses: usize,
    pub proof_psbts: usize,
    pub proof_bytes: u64,
    pub proof_gz_bytes: u64,
    pub consignment_bytes: u64,
    pub consignment_gz_bytes: u64,
    pub import_ms: u128,
    pub receive_validate_ms: u128,
    pub policy_ms: u128,
    pub policy_violations: usize,
    pub renewal_s: f64,
    pub tower_registration_bytes: usize,
    /// threshold: delegate subset used (bitmask) and time spent waiting for co-signers
    #[serde(default)]
    pub subset: u8,
    #[serde(default)]
    pub cosign_ms: u128,
    #[serde(default)]
    pub cosign_calls: u32,
}

pub(crate) fn job_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("jobs").join(format!("{id}.json"))
}

pub(crate) fn out_dir(dir: &Path, id: &str) -> PathBuf {
    dir.join("out").join(id)
}

pub(crate) fn load_job(dir: &Path, id: &str) -> anyhow::Result<Job> {
    Ok(serde_json::from_str(&std::fs::read_to_string(job_path(dir, id))?)?)
}

fn save_job(dir: &Path, j: &Job) -> anyhow::Result<()> {
    let p = job_path(dir, &j.id);
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(j)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

pub(crate) fn ts() -> String {
    String::from_utf8(std::process::Command::new("date").arg("+%H:%M:%S").output().map(|o| o.stdout).unwrap_or_default()).unwrap_or_default().trim().to_string()
}

/// Installs the rust-sdk co-signing hook for one renewal (removed on drop).
struct CosignGuard;

impl CosignGuard {
    fn set(job: &str, addrs: &[String], ms: Arc<std::sync::atomic::AtomicU64>, calls: Arc<std::sync::atomic::AtomicU32>) -> Option<Self> {
        if addrs.is_empty() {
            return None;
        }
        let (job, addrs) = (job.to_string(), addrs.to_vec());
        ark_core::intent::set_rgb_cosign(Some(Arc::new(move |kind: &str, p: &mut bitcoin::Psbt| {
            let d = crate::cosign::cosign_all(&job, kind, p, &addrs, None).map_err(|e| ark_core::Error::ad_hoc(format!("{e:#}")))?;
            ms.fetch_add(d.as_millis() as u64, std::sync::atomic::Ordering::SeqCst);
            calls.fetch_add(addrs.len() as u32, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })));
        Some(CosignGuard)
    }
}

impl Drop for CosignGuard {
    fn drop(&mut self) {
        ark_core::intent::set_rgb_cosign(None);
    }
}

struct Daemon {
    dir: PathBuf,
    regtest: Arc<Regtest>,
    d: Keypair,
    fee_sk: SecretKey,
    rgb: RgbWallet,
    online: Online,
    clients: HashMap<String, ArkClient>,
}

impl Daemon {
    fn log(&self, j: &mut Job, s: &str) {
        let line = format!("{} [{}] {s}", ts(), j.id);
        println!("{line}");
        j.events.push(line);
    }

    async fn client(&mut self, url: &str) -> anyhow::Result<&ArkClient> {
        if !self.clients.contains_key(url) {
            let name = format!("delegate-{}", self.clients.len());
            let (c, _) = crate::common::ark_client_keyed(&name, self.regtest.clone(), &self.dir, url, Some(self.d)).await?;
            self.clients.insert(url.to_string(), c);
        }
        Ok(&self.clients[url])
    }

    fn colored_set(j: &Job) -> HashSet<OutPoint> {
        j.colored_history.iter().chain(std::iter::once(&j.colored)).filter_map(|s| OutPoint::from_str(s).ok()).collect()
    }

    async fn accept(&mut self, j: &mut Job) -> anyhow::Result<()> {
        let url = j.arkd_url.clone();
        let info = self.client(&url).await?.server_info().await?;
        let reg = import(&self.rgb, &self.online, Path::new(&j.proof), &operators(&info))?;
        let rgb = &mut self.rgb;
        let got = bl(|| rgb.ark_receive_vtxo_transfer(self.online.clone(), PathBuf::from(&j.consignment), j.colored.clone())).map_err(|e| anyhow!("{e}"))?;
        ensure!(got == j.units, "delegate sees {got} units on {}, expected {}", j.colored, j.units);
        self.log(j, &format!("accepted: {got} units on {} (proof witnesses {reg:?}); the delegate's RGB wallet now holds a copy of the stash", j.colored));
        j.templates.extend(j.colored_template.clone());
        self.towers(j, OutPoint::from_str(&j.colored)?, j.colored_expires_at, 0).await?;
        j.state = "active".into();
        Ok(())
    }

    async fn towers(&mut self, j: &mut Job, v: OutPoint, expires_at: i64, k: u32) -> anyhow::Result<usize> {
        let url = j.arkd_url.clone();
        let net = self.client(&url).await?.network_client();
        let branch = delegate::exit_branch(&net, &self.regtest, v).await?;
        let n = delegate::register_towers(&self.regtest, &j.towers, &format!("{}-{k}", j.id), v, expires_at, j.tower_threshold_s, &branch, self.fee_sk).await?;
        self.log(j, &format!("towers {:?}: exit branch of {v} ({} txs, {n} B, keyless, fees presigned by the delegate)", j.towers.iter().map(|t| t.file_name().unwrap_or_default().to_string_lossy().to_string()).collect::<Vec<_>>(), branch.len()));
        Ok(n)
    }

    async fn renew(&mut self, j: &mut Job) -> anyhow::Result<()> {
        let t0 = Instant::now();
        let url = j.arkd_url.clone();
        self.client(&url).await?;
        let client = &self.clients[&url];
        let info = client.server_info().await?;
        let net = client.network_client();
        let colored_tpl = j.colored_template.clone().ok_or_else(|| anyhow!("no colored template"))?;
        let spare_tpl = j.spare_template.clone().ok_or_else(|| anyhow!("no spare template"))?;
        // threshold: pick k online delegates and collect their signatures through the rust-sdk hook
        let (leaf, cos_addrs) = match &j.cosigners {
            Some(cs) => {
                let (mask, addrs) = crate::cosign::pick_subset(&j.id, &colored_tpl, cs, j.me)?;
                (Leaf::Subset(mask), addrs)
            }
            None => (Leaf::Delegate, vec![]),
        };
        let cos_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let cos_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let _hook = CosignGuard::set(&j.id, &cos_addrs, cos_ms.clone(), cos_calls.clone());
        let hop_spent = std::cell::Cell::new(Duration::ZERO);
        let colored_set = Self::colored_set(j);
        let k = j.renewals + 1;
        let out = out_dir(&self.dir, &j.id);
        std::fs::create_dir_all(&out)?;
        // 1. spare -> F (fresh batch, colored template with a new T)
        let mut spare_batch = String::new();
        if j.pending_f.is_none() {
            let t_new = delegate::grid_t(now(), j.expiry_s, j.window_s);
            let tpl_f = colored_tpl.with_kind(Kind::Colored(t_new));
            let p = OutPoint::from_str(&j.spare)?;
            let inputs = vec![spare_tpl.intent_input(p, Amount::from_sat(j.spare_sat), leaf, false)];
            let commit = delegate::delegate_settle(client, inputs, tpl_f.address(), &colored_set, Some((&self.rgb, &self.online))).await?;
            let f = delegate::batch_output(&net, tpl_f.address(), commit).await?;
            spare_batch = commit.to_string();
            self.log(j, &format!("renewal {k}: spare {} refreshed via {{D,S}} in batch {commit} -> F {} (T {t_new}, expires in {} s)", j.spare, f.outpoint, f.expires_at - now() as i64));
            j.pending_f = Some((f.outpoint.to_string(), tpl_f, f.expires_at));
            save_job(&self.dir, j)?;
        }
        let (f, tpl_f, f_exp) = j.pending_f.clone().unwrap();
        let f = OutPoint::from_str(&f)?;
        // 2. hop C -> F through CLTV(T){D,S}, the RGB commitment in the checkpoint
        let c = OutPoint::from_str(&j.colored)?;
        let cid = ContractId::from_str(&j.asset_id).map_err(|e| anyhow!("{e}"))?;
        let input = colored_tpl.send_input(c, Amount::from_sat(j.colored_sat), leaf);
        let receivers = vec![SendReceiver::bitcoin(spare_tpl.address(), Amount::from_sat(j.colored_sat))];
        let blinding = rand::random::<u32>() as u64 + 10_000;
        let (cp, ark, transfer) =
            delegate::rgb_hop(&net, &info, &self.rgb, &self.online, input, &receivers, &spare_tpl.address(), std::slice::from_ref(&self.d), cid, vec![(f, j.units, blinding)], j.cosigners.as_ref().map(|_| (j.id.as_str(), cos_addrs.as_slice(), &hop_spent))).await?;
        let cons = out.join(format!("renewal-{k}.rgbc"));
        transfer.save_file(&cons).map_err(|e| anyhow!("save consignment: {e}"))?;
        self.log(j, &format!("renewal {k}: hop {c} -> {f}: checkpoint {cp} (RGB opret, delegate leaf CLTV {}), ark tx {ark}", colored_tpl.cltv().unwrap_or(0)));
        j.colored_history.push(j.colored.clone());
        j.witnesses.push(cp.to_string());
        j.colored = f.to_string();
        j.colored_template = Some(tpl_f.clone());
        // F carries the spare's bitcoin; C's bitcoin (now in the ark tx output) becomes the spare
        let old_colored_sat = j.colored_sat;
        j.colored_sat = j.spare_sat;
        j.spare_sat = old_colored_sat;
        j.colored_expires_at = f_exp;
        j.templates.push(tpl_f.clone());
        j.leftover = Some(OutPoint { txid: ark, vout: 0 }.to_string());
        j.pending_f = None;
        j.renewals = k;
        let prev_proof = PathBuf::from(&j.proof);
        j.consignment = cons.display().to_string();
        j.proof = crate::proof::proof_path(&cons).display().to_string();
        save_job(&self.dir, j)?;
        // 4. proof and handoff for the owner
        let client = &self.clients[&url];
        let pp = PathBuf::from(&j.proof);
        let wit: Vec<Txid> = j.witnesses.iter().filter_map(|w| Txid::from_str(w).ok()).collect();
        delegate::wait_commitments(&net, &self.regtest, f).await?;
        build_proof(client, &self.regtest, &wit, &[prev_proof], &pp).await?;
        add_vtxos(client, &self.regtest, &pp, &[f]).await?;
        let handoff = Handoff {
            asset_id: j.asset_id.clone(),
            current: j.colored.clone(),
            current_template: Some(tpl_f.clone()),
            consignment: j.consignment.clone(),
            proof: j.proof.clone(),
            templates: j.templates.clone(),
            witnesses: j.witnesses.clone(),
        };
        std::fs::write(out.join("handoff.json"), serde_json::to_string_pretty(&handoff)?)?;
        // 3. the ark tx output (C's bitcoin, old expiry) -> next spare
        let leftover = OutPoint::from_str(j.leftover.as_ref().unwrap())?;
        let mut leftover_batch = String::new();
        for attempt in 0..3 {
            let client = &self.clients[&url];
            let inputs = vec![spare_tpl.intent_input(leftover, Amount::from_sat(j.spare_sat), leaf, false)];
            match delegate::delegate_settle(client, inputs, spare_tpl.address(), &Self::colored_set(j), Some((&self.rgb, &self.online))).await {
                Ok(commit) => {
                    let p = delegate::batch_output(&net, spare_tpl.address(), commit).await?;
                    self.log(j, &format!("renewal {k}: leftover {leftover} refreshed in batch {commit} -> spare {}", p.outpoint));
                    j.spare = p.outpoint.to_string();
                    j.leftover = None;
                    leftover_batch = commit.to_string();
                    break;
                }
                Err(e) => self.log(j, &format!("renewal {k}: leftover refresh attempt {attempt} failed: {e:#}")),
            }
        }
        save_job(&self.dir, j)?;
        let renewal_s = t0.elapsed().as_secs_f64();
        // towers first: a failing metrics check below must not leave the new colored VTXO without a fallback
        let tower_bytes = self.towers(j, f, f_exp, k).await.unwrap_or_else(|e| {
            println!("tower registration failed: {e:#}");
            0
        });
        // metrics: a fresh wallet verifies what the owner will verify
        let vdir = out.join(format!("verifier-{k}"));
        std::fs::create_dir_all(&vdir)?;
        // the renewal is already done here; retry the fresh-wallet check on transient indexer errors
        let mut verified = None;
        for attempt in 0..6 {
            let vdir = vdir.join(format!("try-{attempt}"));
            std::fs::create_dir_all(&vdir)?;
            let r: anyhow::Result<(u128, u128, u64)> = async {
                let (mut vw, vonline) = bl(|| crate::rgb_wallet("v", &vdir))?;
                let ti = Instant::now();
                import(&vw, &vonline, &pp, &operators(&info))?;
                let import_ms = ti.elapsed().as_millis();
                let tr = Instant::now();
                let got = bl(|| vw.ark_receive_vtxo_transfer(vonline.clone(), cons.clone(), f.to_string())).map_err(|e| anyhow!("verifier: {e}"))?;
                Ok((import_ms, tr.elapsed().as_millis(), got))
            }
            .await;
            match r {
                Ok(v) => {
                    verified = Some(v);
                    break;
                }
                Err(e) => {
                    self.log(j, &format!("renewal {k}: verifier attempt {attempt} failed (renewal itself done): {}", format!("{e:#}").chars().take(200).collect::<String>()));
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
        let (import_ms, receive_validate_ms, got) = verified.ok_or_else(|| anyhow!("verifier failed 6 times"))?;
        ensure!(got == j.units, "verifier got {got}");
        let tp = Instant::now();
        let rep = delegate::policy_check(&self.regtest, &cons, &pp, &j.templates, delegate::server_xonly(&info))?;
        let policy_ms = tp.elapsed().as_millis();
        let pf: crate::proof::ProofFile = serde_json::from_str(&std::fs::read_to_string(&pp)?)?;
        let m = Metric {
            renewal: k,
            at: now(),
            days_offline: (now() - j.t_offline) as f64 / (j.expiry_s as f64 / 7.0),
            checkpoint: cp.to_string(),
            ark_tx: ark.to_string(),
            spare_batch,
            leftover_batch,
            new_vtxo: f.to_string(),
            expires_in_s: f_exp - now() as i64,
            witnesses: pf.witnesses.len(),
            proof_psbts: pf.psbts.len(),
            proof_bytes: crate::proof::file_len(&pp),
            proof_gz_bytes: delegate::gz_len(&pp),
            consignment_bytes: crate::proof::file_len(&cons),
            consignment_gz_bytes: delegate::gz_len(&cons),
            import_ms,
            receive_validate_ms,
            policy_ms,
            policy_violations: rep.violations.len(),
            renewal_s,
            tower_registration_bytes: tower_bytes,
            subset: match leaf {
                Leaf::Subset(m) => m,
                _ => 0,
            },
            cosign_ms: cos_ms.load(std::sync::atomic::Ordering::SeqCst) as u128 + hop_spent.get().as_millis(),
            cosign_calls: cos_calls.load(std::sync::atomic::Ordering::SeqCst) + if j.cosigners.is_some() { 2 * cos_addrs.len() as u32 } else { 0 },
        };
        if m.subset != 0 {
            self.log(j, &format!("renewal {k}: delegate subset {:03b}, {} co-signer calls, {} ms waiting for co-signers", m.subset, m.cosign_calls, m.cosign_ms));
        }
        self.log(j, &format!("renewal {k} done in {renewal_s:.0} s: proof {} B ({} gz, {} PSBTs), consignment {} B, verify {import_ms}+{receive_validate_ms} ms, policy {} violations", m.proof_bytes, m.proof_gz_bytes, m.proof_psbts, m.consignment_bytes, rep.violations.len()));
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(out.join("metrics.jsonl"))?;
        writeln!(f, "{}", serde_json::to_string(&m)?)?;
        j.failures = 0;
        Ok(())
    }

    async fn step(&mut self, j: &mut Job) -> anyhow::Result<()> {
        match j.state.as_str() {
            "" | "new" => self.accept(j).await,
            "active" => {
                let c = OutPoint::from_str(&j.colored)?;
                if self.regtest.tx_confirmations(&c.txid).is_some() {
                    let probe = vec![j.colored_template.as_ref().unwrap().intent_input(c, Amount::from_sat(j.colored_sat), Leaf::Owner, true)];
                    let g = delegate::guard_batch_inputs(&probe, &Self::colored_set(j), Some((&self.rgb, &self.online)));
                    self.log(j, &format!("colored VTXO {c} is ON-CHAIN (watchtower exit): parked, owner spends it via CSV exit leaf. Re-board attempt: {}", g.err().map(|e| e.to_string()).unwrap_or_else(|| "NOT BLOCKED".into())));
                    j.state = "onchain".into();
                    return Ok(());
                }
                if let Some(l) = j.leftover.clone() {
                    // finish a renewal whose leftover refresh failed
                    self.log(j, &format!("leftover {l} still pending"));
                }
                let t = j.colored_template.as_ref().and_then(|t| t.cltv()).unwrap_or(0) as i64;
                let n = now() as i64;
                if n < t + j.t_margin_s {
                    return Ok(());
                }
                if n > j.colored_expires_at {
                    if j.state != "expired" {
                        self.log(j, "colored VTXO expired without renewal or exit");
                        j.state = "expired".into();
                    }
                    return Ok(());
                }
                match self.renew(j).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        j.failures += 1;
                        let left = j.colored_expires_at - n;
                        let msg = format!(
                            "renewal {} attempt {} REFUSED/FAILED ({left} s to expiry): {}{}",
                            j.renewals + 1,
                            j.failures,
                            format!("{e:#}").chars().take(240).collect::<String>(),
                            if left <= j.tower_threshold_s + 30 { " -> the watchtowers exit before expiry" } else { "" }
                        );
                        self.log(j, &msg);
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        Ok(())
                    }
                }
            }
            _ => Ok(()),
        }
    }
}

pub async fn run_daemon(dir: PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir.join("jobs"))?;
    let regtest = Arc::new(Regtest::new());
    let d = delegate::key_at(&dir.join("delegate-sk.hex"))?;
    let fee_sk = delegate::key_at(&dir.join("fee-sk.hex"))?.secret_key();
    let (rgb, online) = if dir.join("delegate-rgb-mnemonic.txt").exists() {
        bl(|| crate::common::rgb_wallet_load("delegate", &dir))?
    } else {
        bl(|| crate::rgb_wallet("delegate", &dir))?
    };
    println!("{} delegated started (pid {}), D = {}", ts(), std::process::id(), d.x_only_public_key().0);
    let mut dm = Daemon { dir: dir.clone(), regtest, d, fee_sk, rgb, online, clients: HashMap::new() };
    loop {
        let mut ids: Vec<String> = std::fs::read_dir(dir.join("jobs"))?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().to_string()).filter(|_| e.path().extension().is_some_and(|x| x == "json")))
            .collect();
        ids.sort();
        for id in ids {
            let Ok(mut j) = load_job(&dir, &id) else { continue };
            if !matches!(j.state.as_str(), "" | "new" | "active") {
                continue;
            }
            if let Err(e) = dm.step(&mut j).await {
                dm.log(&mut j, &format!("step error: {e:#}"));
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            save_job(&dir, &j)?;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// `ark-rgb-demo proxy <listen> <target>`: plain TCP forwarder. The scenario points one owner's
/// delegate at it and kills it to model an operator that stops cosigning the delegate's spends.
pub async fn run_proxy(listen: String, target: String) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(&listen).await?;
    println!("proxy {listen} -> {target}");
    loop {
        let (mut inb, _) = l.accept().await?;
        let target = target.clone();
        tokio::spawn(async move {
            if let Ok(mut out) = tokio::net::TcpStream::connect(&target).await {
                let _ = tokio::io::copy_bidirectional(&mut inb, &mut out).await;
            }
        });
    }
}

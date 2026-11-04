//! Threshold delegate (closes finding N3 of the single delegate): k-of-n independent delegates.
//!
//! The delegate part of every template is one leaf per k-subset of the delegates, each with S
//! (`CLTV(T){Di,Dj,S}` colored, `{Di,Dj,S}` spare; see [`crate::delegate::Template::threshold`]).
//! arkd accepts that as is: any number of collaborative leaves that each contain S
//! (`TapscriptsVtxoScript.Validate`), and it requires a valid signature for every key of the spent
//! leaf before it finalizes (`verifyTapscriptPartialSigs`). k-of-n CHECKSIGADD is not an option:
//! arkd's `MultisigClosure` only decodes it as n-of-n.
//!
//! One delegate coordinates (it holds the stash and builds the txs); every other delegate runs
//! `ark-rgb-demo cosignd <dir>`, holds only its own key, and signs a PSBT only if its own checks
//! pass:
//!
//! * `intent` (batch intent proof) and `forfeit`: every spent VTXO is the owner's spare template
//!   (never a colored one: no asset ever enters a batch, #961), every output is a template of the
//!   same owner and delegate set, forfeits pay the operator's forfeit address.
//! * `ark` / `checkpoint` (the renewal hop): the checkpoint spends a colored template of the job
//!   through a delegate subset leaf; its `OP_RETURN` is the RGB anchor of the bundle in the
//!   consignment (`Anchor::verify`, so the coordinator cannot show one transition and commit to
//!   another); every transition of the bundle is revealed and every seal is an explicit outpoint
//!   whose VTXO (looked up in arkd's indexer) is a colored template of the same owner and
//!   delegates; the ark tx pays only the spare template.
//!
//! A single delegate therefore cannot move the asset anywhere else; k colluding delegates can.
use crate::delegate::{Kind, T_GRID_S, Template};
use crate::{bl, now};
use anyhow::{anyhow, bail, ensure};
use base64::Engine;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::Message;
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::taproot::{self, TapLeafHash};
use bitcoin::{OutPoint, Psbt, ScriptBuf, TapSighashType, Txid, XOnlyPublicKey};

use rgb_lib::{ConsignmentExt, ContractId, FileContent, RgbTransfer};
use rgbcore::Operation;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

/// What a co-signer knows about one owner (no secrets): the base template (any kind) and timing.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(crate) struct CosignJob {
    pub id: String,
    pub base: Template,
    pub asset_id: String,
    pub expiry_s: u64,
    pub arkd_url: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub(crate) struct Request {
    pub job: String,
    /// "ping" | "intent" | "forfeit" | "ark" | "checkpoint"
    pub kind: String,
    /// base64 PSBT to sign
    #[serde(default)]
    pub psbt: String,
    /// hop context: the checkpoint and ark tx (base64 PSBTs) and the consignment (base64 file)
    #[serde(default)]
    pub checkpoint: String,
    #[serde(default)]
    pub ark: String,
    #[serde(default)]
    pub consignment: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub(crate) struct Response {
    pub ok: bool,
    #[serde(default)]
    pub psbt: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub pubkey: String,
}

pub(crate) fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn unb64(s: &str) -> anyhow::Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

pub(crate) fn psbt_b64(p: &Psbt) -> String {
    b64(&p.serialize())
}

fn psbt_of(s: &str) -> anyhow::Result<Psbt> {
    Ok(Psbt::deserialize(&unb64(s)?)?)
}

/// One request to the co-signer at `addr` (`host:port`); returns the PSBT with its signatures.
pub(crate) fn call(addr: &str, req: &Request) -> anyhow::Result<Response> {
    let sa = addr.parse().map_err(|e| anyhow!("{addr}: {e}"))?;
    let mut s = std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(3))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    let mut out = String::new();
    BufReader::new(s).read_line(&mut out)?;
    let r: Response = serde_json::from_str(&out)?;
    if !r.ok {
        bail!("co-signer {addr} refused {}: {}", req.kind, r.error);
    }
    Ok(r)
}

/// Merge the co-signer's `tap_script_sigs` into `into`.
pub(crate) fn merge_sigs(into: &mut Psbt, from: &Psbt) -> anyhow::Result<()> {
    ensure!(into.unsigned_tx.compute_txid() == from.unsigned_tx.compute_txid(), "co-signer returned another tx");
    for (a, b) in into.inputs.iter_mut().zip(from.inputs.iter()) {
        for (k, v) in &b.tap_script_sigs {
            a.tap_script_sigs.insert(*k, *v);
        }
    }
    Ok(())
}

/// Leaf script of input `i` (PSBT fields).
fn leaf_of(p: &Psbt, i: usize) -> Option<(ScriptBuf, taproot::LeafVersion)> {
    p.inputs[i].tap_scripts.values().next().cloned().or_else(|| p.inputs[i].witness_script.clone().map(|s| (s, taproot::LeafVersion::TapScript)))
}

fn has_key(script: &ScriptBuf, k: XOnlyPublicKey) -> bool {
    script.as_bytes().windows(32).any(|w| w == k.serialize())
}

/// Sign (SIGHASH_DEFAULT) every input whose leaf holds `kp`'s key. Returns how many.
fn sign_mine(p: &mut Psbt, kp: &Keypair) -> anyhow::Result<usize> {
    let me = kp.x_only_public_key().0;
    let prevouts: Vec<_> = p.inputs.iter().map(|i| i.witness_utxo.clone().ok_or_else(|| anyhow!("missing witness utxo"))).collect::<Result<_, _>>()?;
    let secp = Secp256k1::new();
    let mut n = 0;
    for i in 0..p.inputs.len() {
        let Some((script, ver)) = leaf_of(p, i) else { continue };
        if !has_key(&script, me) {
            continue;
        }
        let lh = TapLeafHash::from_script(&script, ver);
        let sh = SighashCache::new(&p.unsigned_tx).taproot_script_spend_signature_hash(i, &Prevouts::All(&prevouts), lh, TapSighashType::Default)?;
        let sig = secp.sign_schnorr_no_aux_rand(&Message::from_digest(sh.to_raw_hash().to_byte_array()), kp);
        p.inputs[i].tap_script_sigs.insert((me, lh), taproot::Signature { signature: sig, sighash_type: TapSighashType::Default });
        n += 1;
    }
    Ok(n)
}

use bitcoin::hashes::Hash;

/// Templates of the job around now: the spare and the colored ones on the T grid.
fn candidates(job: &CosignJob) -> (Template, Vec<Template>) {
    let n = now();
    let from = n.saturating_sub(2 * job.expiry_s) / T_GRID_S * T_GRID_S;
    let to = n + 2 * job.expiry_s;
    let mut v = vec![];
    let mut t = from;
    while t <= to {
        v.push(job.base.with_kind(Kind::Colored(t as u32)));
        t += T_GRID_S;
    }
    (job.base.with_kind(Kind::Spare), v)
}

struct Cosigner {
    kp: Keypair,
    dir: PathBuf,
}

impl Cosigner {
    fn job(&self, id: &str) -> anyhow::Result<CosignJob> {
        ensure!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "bad job id");
        Ok(serde_json::from_str(&std::fs::read_to_string(self.dir.join("jobs").join(format!("{id}.json")))?)?)
    }

    async fn handle(&self, req: Request) -> anyhow::Result<Response> {
        let me = self.kp.x_only_public_key().0;
        if req.kind == "ping" {
            if self.dir.join(format!("offline-{}", req.job)).exists() {
                bail!("offline");
            }
            return Ok(Response { ok: true, pubkey: me.to_string(), ..Default::default() });
        }
        ensure!(!self.dir.join(format!("offline-{}", req.job)).exists(), "offline");
        let job = self.job(&req.job)?;
        ensure!(job.base.delegates.iter().any(|d| d == &me.to_string()), "not a delegate of {}", job.id);
        let (spare, colored) = candidates(&job);
        let colored_spks: Vec<ScriptBuf> = colored.iter().map(|t| t.spk()).collect();
        let ours = |spk: &ScriptBuf| *spk == spare.spk() || colored_spks.contains(spk);
        let mut p = psbt_of(&req.psbt)?;
        match req.kind.as_str() {
            "intent" => {
                for (i, inp) in p.inputs.iter().enumerate() {
                    let spk = inp.witness_utxo.as_ref().ok_or_else(|| anyhow!("intent input {i}: no prevout"))?.script_pubkey.clone();
                    ensure!(spk == spare.spk(), "intent input {i} is not the owner's spare template (colored or foreign VTXOs never join a batch)");
                }
                for (i, o) in p.unsigned_tx.output.iter().enumerate() {
                    ensure!(o.script_pubkey.is_op_return() || ours(&o.script_pubkey), "intent output {i} pays a foreign script");
                }
            }
            "forfeit" => {
                let info = self.arkd(&job).await?.get_info().await.map_err(|e| anyhow!("{e}"))?;
                ensure!(p.inputs.len() == 2, "forfeit with {} inputs", p.inputs.len());
                let spk = p.inputs[1].witness_utxo.as_ref().ok_or_else(|| anyhow!("forfeit: no prevout"))?.script_pubkey.clone();
                ensure!(spk == spare.spk(), "forfeit of a VTXO that is not the owner's spare template");
                ensure!(p.unsigned_tx.output[0].script_pubkey == info.forfeit_address.script_pubkey(), "forfeit does not pay the operator's forfeit address");
            }
            "ark" | "checkpoint" => {
                self.check_hop(&job, &req, &spare, &colored).await?;
            }
            k => bail!("unknown request {k}"),
        }
        let n = sign_mine(&mut p, &self.kp)?;
        ensure!(n > 0, "no input of this {} is mine to sign", req.kind);
        Ok(Response { ok: true, psbt: psbt_b64(&p), pubkey: me.to_string(), ..Default::default() })
    }

    async fn arkd(&self, job: &CosignJob) -> anyhow::Result<ark_grpc::Client> {
        let mut c = ark_grpc::Client::new(job.arkd_url.clone());
        c.connect().await.map_err(|e| anyhow!("arkd: {e}"))?;
        Ok(c)
    }

    /// The renewal hop: see the module docs.
    async fn check_hop(&self, job: &CosignJob, req: &Request, spare: &Template, colored: &[Template]) -> anyhow::Result<()> {
        let cp = psbt_of(&req.checkpoint)?;
        let ark = psbt_of(&req.ark)?;
        let cp_txid = cp.unsigned_tx.compute_txid();
        // the PSBT to sign is one of the two
        let target = psbt_of(&req.psbt)?.unsigned_tx.compute_txid();
        ensure!(target == cp_txid || target == ark.unsigned_tx.compute_txid(), "PSBT is neither the checkpoint nor the ark tx");
        // checkpoint: spends a colored template through a delegate subset leaf
        ensure!(cp.inputs.len() == 1, "checkpoint with {} inputs", cp.inputs.len());
        let spent = cp.inputs[0].witness_utxo.as_ref().ok_or_else(|| anyhow!("checkpoint: no prevout"))?.script_pubkey.clone();
        let tpl = colored.iter().find(|t| t.spk() == spent).ok_or_else(|| anyhow!("checkpoint spends no colored template of {}", job.id))?;
        let (leaf, _) = leaf_of(&cp, 0).ok_or_else(|| anyhow!("checkpoint: no leaf"))?;
        ensure!(tpl.delegate_leaves().contains(&leaf), "checkpoint does not use a delegate subset leaf");
        // ark tx: spends the checkpoint, pays only the spare template
        ensure!(ark.unsigned_tx.input.iter().all(|i| i.previous_output.txid == cp_txid), "ark tx spends something else");
        for (i, o) in ark.unsigned_tx.output.iter().enumerate() {
            let anchor = o.value.to_sat() == 0 && !o.script_pubkey.is_op_return();
            ensure!(anchor || o.script_pubkey.is_op_return() || o.script_pubkey == spare.spk(), "ark tx output {i} pays a foreign script");
        }
        // RGB: the checkpoint commits to this bundle; every seal on a template of the same parties
        let dir = std::env::temp_dir().join(format!("cosign-{}-{}", std::process::id(), rand::random::<u32>()));
        std::fs::create_dir_all(&dir)?;
        let f = dir.join("c.rgbc");
        std::fs::write(&f, unb64(&req.consignment)?)?;
        let t = RgbTransfer::load_file(&f).map_err(|e| anyhow!("consignment: {e}"))?;
        let _ = std::fs::remove_dir_all(&dir);
        let cid = ContractId::from_str(&job.asset_id).map_err(|e| anyhow!("{e}"))?;
        ensure!(t.contract_id() == cid, "consignment is for another asset");
        let wb = t.bundles.iter().find(|b| b.witness_id().to_string() == cp_txid.to_string()).ok_or_else(|| anyhow!("no bundle for checkpoint {cp_txid}"))?;
        wb.anchor.verify(cid, wb.bundle.bundle_id(), &cp.unsigned_tx).map_err(|e| anyhow!("checkpoint does not commit to the shown bundle: {e}"))?;
        let opids: std::collections::BTreeSet<_> = wb.bundle.input_map.values().copied().collect();
        let known: std::collections::BTreeSet<_> = wb.bundle.known_transitions.iter().map(|k| k.opid).collect();
        ensure!(opids == known, "bundle has unrevealed transitions");
        let net = self.arkd(job).await?;
        let mut seals = 0;
        for kt in wb.bundle.known_transitions.iter() {
            ensure!(kt.transition.id() == kt.opid, "transition id mismatch");
            for (_, assigns) in kt.transition.assignments.iter() {
                for a in assigns.as_fungible() {
                    let seal = a.revealed_seal().ok_or_else(|| anyhow!("concealed seal"))?;
                    let txid = Txid::from_str(&seal.txid.to_string()).map_err(|_| anyhow!("seal on the witness tx itself, not on an owner template"))?;
                    let op = OutPoint { txid, vout: seal.vout.to_string().parse()? };
                    let v = crate::delegate::vtxo_at(&net, op).await.ok_or_else(|| anyhow!("seal {op} is no VTXO known to arkd"))?;
                    ensure!(colored.iter().any(|c| c.same_parties(tpl) && c.spk() == v.script), "seal {op} is not a colored template of this owner and delegate set");
                    seals += 1;
                }
            }
        }
        ensure!(seals > 0, "no seal to check");
        Ok(())
    }
}

/// `ark-rgb-demo cosignd <dir>`: `<dir>/key.hex` (created), `<dir>/listen.txt` (`host:port`),
/// `<dir>/jobs/<id>.json` ([`CosignJob`]); a file `<dir>/offline-<id>` makes it refuse that job.
pub(crate) async fn run_daemon(dir: PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir.join("jobs"))?;
    let kp = crate::delegate::key_at(&dir.join("key.hex"))?;
    let listen = std::fs::read_to_string(dir.join("listen.txt"))?.trim().to_string();
    let l = tokio::net::TcpListener::bind(&listen).await?;
    println!("cosignd {} on {listen}, key {}", dir.display(), kp.x_only_public_key().0);
    let me = std::sync::Arc::new(Cosigner { kp, dir });
    loop {
        let (s, _) = l.accept().await?;
        let me = me.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let (r, mut w) = s.into_split();
            let mut line = String::new();
            if tokio::io::BufReader::new(r).read_line(&mut line).await.is_err() {
                return;
            }
            let t0 = Instant::now();
            let resp = match serde_json::from_str::<Request>(&line) {
                Ok(req) => {
                    let (job, kind) = (req.job.clone(), req.kind.clone());
                    let r = me.handle(req).await;
                    let ms = t0.elapsed().as_millis();
                    match r {
                        Ok(r) => {
                            if kind != "ping" {
                                println!("{} {job} {kind}: signed ({ms} ms)", crate::delegated::ts());
                            }
                            r
                        }
                        Err(e) => {
                            println!("{} {job} {kind}: REFUSED ({ms} ms): {e:#}", crate::delegated::ts());
                            Response { ok: false, error: format!("{e:#}"), ..Default::default() }
                        }
                    }
                }
                Err(e) => Response { ok: false, error: format!("bad request: {e}"), ..Default::default() },
            };
            let mut out = serde_json::to_string(&resp).unwrap_or_default();
            out.push('\n');
            let _ = w.write_all(out.as_bytes()).await;
        });
    }
}

/// Coordinator side: the co-signers of a threshold job and the subset in use.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub(crate) struct Cosigners {
    /// per delegate index (same order as `Template::delegates`): `host:port`, empty for the coordinator
    pub addrs: Vec<String>,
}

/// Pick a k-subset: the coordinator (index `me`) plus the first k-1 co-signers that answer a ping.
pub(crate) fn pick_subset(job: &str, tpl: &Template, cs: &Cosigners, me: usize) -> anyhow::Result<(u8, Vec<String>)> {
    let mut mask = 1u8 << me;
    let mut addrs = vec![];
    for (i, a) in cs.addrs.iter().enumerate() {
        if i == me || a.is_empty() || mask.count_ones() as u8 >= tpl.k {
            continue;
        }
        if bl(|| call(a, &Request { job: job.to_string(), kind: "ping".into(), ..Default::default() })).is_ok() {
            mask |= 1 << i;
            addrs.push(a.clone());
        }
    }
    ensure!(mask.count_ones() as u8 >= tpl.k, "only {} of {} delegates online (k = {})", mask.count_ones(), tpl.delegates.len(), tpl.k);
    Ok((mask, addrs))
}

/// Ask every co-signer in `addrs` to sign `p` (merging their signatures). Returns the time spent.
pub(crate) fn cosign_all(job: &str, kind: &str, p: &mut Psbt, addrs: &[String], hop: Option<(&Psbt, &Psbt, &Path)>) -> anyhow::Result<Duration> {
    let t0 = Instant::now();
    let mut req = Request { job: job.to_string(), kind: kind.to_string(), psbt: psbt_b64(p), ..Default::default() };
    if let Some((cp, ark, cons)) = hop {
        req.checkpoint = psbt_b64(cp);
        req.ark = psbt_b64(ark);
        req.consignment = b64(&std::fs::read(cons)?);
    }
    for a in addrs {
        let r = call(a, &req)?;
        merge_sigs(p, &psbt_of(&r.psbt)?)?;
    }
    Ok(t0.elapsed())
}


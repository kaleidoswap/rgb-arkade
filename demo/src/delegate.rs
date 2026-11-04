//! Delegate-key renewal (operator-risk option O2) for RGB assets in Arkade VTXOs, on an arkd with
//! only the checkpoint OP_RETURN change (B). Nothing here needs another arkd change.
//!
//! Every receive address of the owner is one of two VTXO templates (A = owner, D = delegate,
//! S = operator signer):
//!
//! * colored: `{S,A}`, `CSV(exit){A}`, `CLTV(T){D,S}`. The delegate leaf opens at `T`, shortly
//!   before the VTXO's batch expires; before `T` only the owner (with S) can move it.
//! * spare (uncolored bitcoin): `{S,A}`, `CSV(exit){A}`, `{D,S}`. The rust-sdk builds intent
//!   proofs and forfeits with nLockTime 0, so a CLTV leaf cannot be refreshed in a batch.
//!
//! One renewal, all signed by D (the delegate holds no owner key):
//!   1. batch: the spare P is refreshed through `{D,S}` into F, a colored template with a new `T`
//!   2. off-chain hop: the checkpoint of the colored VTXO C spends it through `CLTV(T){D,S}` and
//!      carries the RGB commitment; the transition moves the asset to the explicit seal F. The ark
//!      tx returns C's bitcoin to the spare template (O)
//!   3. batch: O (uncolored now) is refreshed into the next spare
//!
//! An RGB transition needs no owner signature (the NIA transfer script only checks that inputs
//! and outputs sum up; only schemas using `vts`, e.g. PFA, check a signature, and that is the
//! issuer's), so the delegate's checkpoint closes the seal like the owner's would.
//!
//! `T` sits on a fixed grid (`T_GRID_S`), so the owner finds F again from A, D and S alone by
//! scanning the grid addresses in arkd's indexer ([`discover`]).
//!
//! The protocol does not stop D (with S, which cosigns any valid spend) from moving the asset
//! elsewhere once `T` has passed. [`policy_check`] is the receiver-side rule that makes such a
//! move unacceptable to honest wallets: a spend through a CLTV `{x,S}` leaf must assign every
//! asset seal to a colored template of the same owner. It is a wallet rule, not RGB or Ark
//! consensus.
use crate::regtest::Regtest;
use crate::watchtowerd::{FeeKey, Registration, registration_dir};
use crate::{ArkClient, bl};
use anyhow::{anyhow, bail, ensure};
use ark_core::server::{GetVtxosRequest, Info, VirtualTxOutPoint};
use ark_core::send::{OffchainTransactions, SendReceiver, VtxoInput, build_offchain_transactions, sign_ark_transaction, sign_checkpoint_transaction};
use ark_core::{ArkAddress, Vtxo, intent};
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::opcodes::all::{OP_CHECKSIG, OP_CHECKSIGVERIFY, OP_CLTV, OP_DROP};
use bitcoin::script::Instruction;
use bitcoin::secp256k1::{Message, SecretKey, schnorr};
use bitcoin::{Address, Amount, Network, OutPoint, Psbt, ScriptBuf, Sequence, Transaction, TxOut, Txid, XOnlyPublicKey, absolute, psbt};
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::{Online, Outpoint, Wallet as RgbWallet};
use rgb_lib::{ContractId, FileContent, RgbTransfer};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

/// Grid of the delegate leaf's CLTV `T` (unix seconds): the owner scans it to find renewed VTXOs.
pub(crate) const T_GRID_S: u64 = 64;

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `CLTV(T){D,S}`: holds the asset
    Colored(u32),
    /// `{D,S}`: the owner's bitcoin the delegate refreshes and moves the asset onto
    Spare,
    /// negative test only: a delegate leaf `{D}` without the operator key
    NoSigner,
}

/// A receive template of one owner. Keys are x-only hex.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Template {
    pub owner: String,
    pub delegate: String,
    pub server: String,
    /// unilateral exit delay (BIP68 sequence, consensus encoding)
    pub exit_delay: u32,
    pub kind: Kind,
    /// threshold delegate: all n delegate keys (x-only hex) and k. Empty = single delegate `delegate`.
    /// Then the delegate part is one leaf per k-subset `{Di..Dj,S}` (CLTV(T) for colored): arkd
    /// accepts any number of collaborative leaves as long as each contains S, but CHECKSIGADD only as
    /// n-of-n, so k-of-n is spelled out as subsets.
    #[serde(default)]
    pub delegates: Vec<String>,
    #[serde(default)]
    pub k: u8,
}

pub(crate) fn xonly(h: &str) -> anyhow::Result<XOnlyPublicKey> {
    Ok(XOnlyPublicKey::from_str(h)?)
}

pub(crate) fn server_xonly(info: &Info) -> XOnlyPublicKey {
    info.signer_pk.x_only_public_key().0
}

/// `<k1> CHECKSIGVERIFY .. <kn> CHECKSIG` (arkd `MultisigClosure`, CHECKSIG form)
pub(crate) fn multisig(keys: &[XOnlyPublicKey]) -> ScriptBuf {
    let mut b = ScriptBuf::builder();
    for (i, k) in keys.iter().enumerate() {
        b = b.push_x_only_key(k).push_opcode(if i + 1 == keys.len() { OP_CHECKSIG } else { OP_CHECKSIGVERIFY });
    }
    b.into_script()
}

/// `<T> CLTV DROP` + [`multisig`] (arkd `CLTVMultisigClosure`)
pub(crate) fn cltv_multi(t: u32, keys: &[XOnlyPublicKey]) -> ScriptBuf {
    let mut v = ScriptBuf::builder().push_int(t as i64).push_opcode(OP_CLTV).push_opcode(OP_DROP).into_script().into_bytes();
    v.extend(multisig(keys).into_bytes());
    ScriptBuf::from_bytes(v)
}

/// All k-subsets of 0..n as bitmasks, in increasing order.
pub(crate) fn subsets(n: usize, k: usize) -> Vec<u8> {
    (0u16..(1 << n)).filter(|m| m.count_ones() as usize == k).map(|m| m as u8).collect()
}

/// `<T> CLTV DROP <k1> CHECKSIGVERIFY <k2> CHECKSIG` (arkd `CLTVMultisigClosure`)
pub(crate) fn cltv_multisig(t: u32, k1: XOnlyPublicKey, k2: XOnlyPublicKey) -> ScriptBuf {
    ScriptBuf::builder()
        .push_int(t as i64)
        .push_opcode(OP_CLTV)
        .push_opcode(OP_DROP)
        .push_x_only_key(&k1)
        .push_opcode(OP_CHECKSIGVERIFY)
        .push_x_only_key(&k2)
        .push_opcode(OP_CHECKSIG)
        .into_script()
}

impl Template {
    pub fn new(owner: XOnlyPublicKey, delegate: XOnlyPublicKey, info: &Info, kind: Kind) -> Self {
        Self {
            owner: owner.to_string(),
            delegate: delegate.to_string(),
            server: server_xonly(info).to_string(),
            exit_delay: info.unilateral_exit_delay.to_consensus_u32(),
            kind,
            delegates: vec![],
            k: 0,
        }
    }
    /// k-of-n threshold template; `delegate` is set to the first key (the coordinator).
    pub fn threshold(owner: XOnlyPublicKey, delegates: &[XOnlyPublicKey], k: u8, info: &Info, kind: Kind) -> Self {
        Self { delegates: delegates.iter().map(|d| d.to_string()).collect(), k, ..Self::new(owner, delegates[0], info, kind) }
    }
    pub fn is_threshold(&self) -> bool {
        !self.delegates.is_empty()
    }
    /// same owner and same delegate set
    pub fn same_parties(&self, o: &Template) -> bool {
        self.owner == o.owner && self.delegate == o.delegate && self.delegates == o.delegates && self.k == o.k
    }
    fn dkeys(&self) -> Vec<XOnlyPublicKey> {
        self.delegates.iter().map(|d| xonly(d).unwrap()).collect()
    }
    /// The delegate leaf of the subset `mask` (threshold templates).
    pub fn subset_leaf(&self, mask: u8) -> ScriptBuf {
        let (_, _, s) = self.keys();
        let mut keys: Vec<XOnlyPublicKey> = self.dkeys().into_iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, k)| k).collect();
        keys.push(s);
        match self.kind {
            Kind::Colored(t) => cltv_multi(t, &keys),
            _ => multisig(&keys),
        }
    }
    /// Every delegate leaf (one, or one per k-subset).
    pub fn delegate_leaves(&self) -> Vec<ScriptBuf> {
        if self.is_threshold() {
            subsets(self.delegates.len(), self.k as usize).into_iter().map(|m| self.subset_leaf(m)).collect()
        } else {
            vec![self.delegate_leaf()]
        }
    }
    pub fn with_kind(&self, kind: Kind) -> Self {
        Self { kind, ..self.clone() }
    }
    fn keys(&self) -> (XOnlyPublicKey, XOnlyPublicKey, XOnlyPublicKey) {
        (xonly(&self.owner).unwrap(), xonly(&self.delegate).unwrap(), xonly(&self.server).unwrap())
    }
    pub fn cltv(&self) -> Option<u32> {
        match self.kind {
            Kind::Colored(t) => Some(t),
            _ => None,
        }
    }
    pub fn owner_leaf(&self) -> ScriptBuf {
        let (a, _, s) = self.keys();
        ark_core::script::multisig_script(s, a)
    }
    pub fn exit_leaf(&self) -> ScriptBuf {
        let (a, _, _) = self.keys();
        ark_core::script::csv_sig_script(Sequence::from_consensus(self.exit_delay), a)
    }
    pub fn delegate_leaf(&self) -> ScriptBuf {
        let (_, d, s) = self.keys();
        match self.kind {
            Kind::Colored(t) => cltv_multisig(t, d, s),
            Kind::Spare => ark_core::script::multisig_script(d, s),
            Kind::NoSigner => ScriptBuf::builder().push_x_only_key(&d).push_opcode(OP_CHECKSIG).into_script(),
        }
    }
    pub fn vtxo(&self) -> Vtxo {
        let (a, _, s) = self.keys();
        Vtxo::new_with_custom_scripts(
            &Secp256k1::new(),
            s,
            a,
            [vec![self.owner_leaf(), self.exit_leaf()], self.delegate_leaves()].concat(),
            Sequence::from_consensus(self.exit_delay),
            Network::Regtest,
        )
        .expect("template")
    }
    pub fn spk(&self) -> ScriptBuf {
        self.vtxo().script_pubkey()
    }
    pub fn address(&self) -> ArkAddress {
        self.vtxo().to_ark_address()
    }
    fn leaf(&self, which: Leaf) -> ScriptBuf {
        match which {
            Leaf::Owner => self.owner_leaf(),
            Leaf::Delegate => self.delegate_leaf(),
            Leaf::Subset(m) => self.subset_leaf(m),
        }
    }
    /// Input of an off-chain send spending `outpoint` (of this template) through `which`.
    pub fn send_input(&self, outpoint: OutPoint, amount: Amount, which: Leaf) -> VtxoInput {
        let v = self.vtxo();
        let script = self.leaf(which);
        let cb = v.get_spend_info(script.clone()).expect("leaf in template");
        let locktime = match (which, self.kind) {
            (Leaf::Delegate | Leaf::Subset(_), Kind::Colored(t)) => Some(absolute::LockTime::from_time(t).expect("unix time")),
            _ => None,
        };
        VtxoInput::new(script, locktime, cb, v.tapscripts(), v.script_pubkey(), amount, outpoint, vec![])
    }
    /// Input of a batch intent spending `outpoint` through `which` (no CLTV: spare template only).
    pub fn intent_input(&self, outpoint: OutPoint, amount: Amount, which: Leaf, onchain: bool) -> intent::Input {
        let v = self.vtxo();
        let script = self.leaf(which);
        let cb = v.get_spend_info(script.clone()).expect("leaf in template");
        intent::Input::new(
            outpoint,
            Sequence::from_consensus(self.exit_delay),
            None,
            TxOut { value: amount, script_pubkey: v.script_pubkey() },
            v.tapscripts(),
            (script, cb),
            onchain,
            false,
            vec![],
        )
    }
    /// On-chain spend of an exited output of this template through the owner's exit leaf.
    pub fn exit_input(&self, outpoint: OutPoint, amount: Amount) -> ark_core::unilateral_exit::VtxoInput {
        let v = self.vtxo();
        let script = self.exit_leaf();
        let cb = v.get_spend_info(script.clone()).expect("exit leaf");
        ark_core::unilateral_exit::VtxoInput::new(
            outpoint,
            Sequence::from_consensus(self.exit_delay),
            TxOut { value: amount, script_pubkey: v.script_pubkey() },
            (script, cb),
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Leaf {
    Owner,
    Delegate,
    /// threshold template: the leaf of this k-subset of the delegates (bitmask)
    Subset(u8),
}

/// `T` for a colored VTXO created now: `window` before its expected expiry, on the grid.
pub(crate) fn grid_t(now: u64, expiry_s: u64, window_s: u64) -> u32 {
    ((now + expiry_s - window_s) / T_GRID_S * T_GRID_S) as u32
}

pub(crate) fn save_key(path: &Path) -> anyhow::Result<Keypair> {
    let sk = SecretKey::new(&mut bitcoin::secp256k1::rand::thread_rng());
    std::fs::write(path, hex::encode(sk.secret_bytes()))?;
    Ok(Keypair::from_secret_key(&Secp256k1::new(), &sk))
}

pub(crate) fn load_key(path: &Path) -> anyhow::Result<Keypair> {
    let sk = SecretKey::from_slice(&hex::decode(std::fs::read_to_string(path)?.trim())?)?;
    Ok(Keypair::from_secret_key(&Secp256k1::new(), &sk))
}

pub(crate) fn key_at(path: &Path) -> anyhow::Result<Keypair> {
    if path.exists() { load_key(path) } else { save_key(path) }
}

type SignFn = Box<dyn Fn(&mut psbt::Input, Message) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> + Send + Sync>;

/// Schnorr-sign whatever leaf the PSBT input names with one key.
pub(crate) fn signer(kp: Keypair) -> SignFn {
    Box::new(move |_: &mut psbt::Input, msg: Message| {
        let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
        Ok(vec![(sig, kp.x_only_public_key().0)])
    })
}

/// Schnorr-sign with every key of `kps` that the PSBT input's leaf names (colluding-delegates test).
pub(crate) fn signer_multi(kps: &[Keypair]) -> SignFn {
    let kps = kps.to_vec();
    Box::new(move |i: &mut psbt::Input, msg: Message| {
        let leaf = i.tap_scripts.values().next().map(|(s, _)| s.clone()).or_else(|| i.witness_script.clone());
        let secp = Secp256k1::new();
        Ok(kps
            .iter()
            .filter(|kp| kps.len() == 1 || leaf.as_ref().is_none_or(|l| l.as_bytes().windows(32).any(|w| w == kp.x_only_public_key().0.serialize())))
            .map(|kp| (secp.sign_schnorr_no_aux_rand(&msg, kp), kp.x_only_public_key().0))
            .collect())
    })
}

/// Threshold hop: the co-signers to ask (besides `kp`), with the consignment they check.
pub(crate) struct Cosign<'a> {
    pub job: &'a str,
    pub addrs: &'a [String],
    pub consignment: &'a Path,
    /// time spent waiting for co-signers
    pub spent: std::cell::Cell<Duration>,
}

/// Build, sign (with `kp` only), submit and finalize an off-chain tx spending one VTXO input.
/// `payload` is the checkpoint OP_RETURN (B). Returns (checkpoint txid, ark txid).
pub(crate) async fn submit_hop(
    net: &ark_grpc::Client,
    info: &Info,
    input: VtxoInput,
    receivers: &[SendReceiver],
    change: &ArkAddress,
    kps: &[Keypair],
    payload: Option<[u8; 32]>,
    cos: Option<&Cosign<'_>>,
) -> anyhow::Result<(Txid, Txid)> {
    let OffchainTransactions { mut ark_tx, checkpoint_txs } = build_offchain_transactions(receivers, change, &[input.with_checkpoint_op_return(payload)], info)?;
    let cp_txid = checkpoint_txs[0].unsigned_tx.compute_txid();
    sign_ark_transaction(signer_multi(kps), &mut ark_tx, 0)?;
    if let Some(c) = cos {
        let ark0 = ark_tx.clone();
        let d = bl(|| crate::cosign::cosign_all(c.job, "ark", &mut ark_tx, c.addrs, Some((&checkpoint_txs[0], &ark0, c.consignment))))?;
        c.spent.set(c.spent.get() + d);
    }
    let res = net.submit_offchain_transaction_request(ark_tx, checkpoint_txs).await.map_err(|e| anyhow!("submit: {e}"))?;
    let ark_txid = res.signed_ark_tx.unsigned_tx.compute_txid();
    let mut cps = res.signed_checkpoint_txs;
    for cp in cps.iter_mut() {
        if cp.inputs[0].witness_script.is_none() {
            cp.inputs[0].witness_script = res.signed_ark_tx.inputs[0].witness_script.clone();
        }
        sign_checkpoint_transaction(signer_multi(kps), cp)?;
        if let Some(c) = cos {
            let cp0 = cp.clone();
            let d = bl(|| crate::cosign::cosign_all(c.job, "checkpoint", cp, c.addrs, Some((&cp0, &res.signed_ark_tx, c.consignment))))?;
            c.spent.set(c.spent.get() + d);
        }
    }
    net.finalize_offchain_transaction(ark_txid, cps).await.map_err(|e| anyhow!("finalize: {e}"))?;
    Ok((cp_txid, ark_txid))
}

/// [`submit_hop`] whose checkpoint carries the RGB transition moving all of `units` (held by `rgb`
/// on the input) to the explicit `seals`. On any failure the transition is rolled back.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn rgb_hop(
    net: &ark_grpc::Client,
    info: &Info,
    rgb: &RgbWallet,
    online: &Online,
    input: VtxoInput,
    receivers: &[SendReceiver],
    change: &ArkAddress,
    kps: &[Keypair],
    cid: ContractId,
    seals: Vec<(OutPoint, u64, u64)>,
    cos: Option<(&str, &[String], &std::cell::Cell<Duration>)>,
) -> anyhow::Result<(Txid, Txid, RgbTransfer)> {
    let dry = build_offchain_transactions(receivers, change, &[input.clone().with_checkpoint_op_return(Some([0u8; 32]))], info)?;
    let mut cp = dry.checkpoint_txs[0].clone();
    cp.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    let seals: Vec<(SealTarget, u64, u64)> =
        seals.into_iter().map(|(o, u, b)| (SealTarget::Explicit(Outpoint { txid: o.txid.to_string(), vout: o.vout }), u, b)).collect();
    let transfer = bl(|| rgb.color_psbt_and_consume_custom(&mut cp, cid, seals))?;
    let cp_txid = cp.unsigned_tx.compute_txid();
    let commitment = cp.unsigned_tx.output[0].script_pubkey.clone();
    let res = async {
        ensure!(commitment.is_op_return() && commitment.len() == 34, "bad commitment {commitment}");
        let mut payload = [0u8; 32];
        payload.copy_from_slice(&commitment.as_bytes()[2..]);
        let tmp = std::env::temp_dir().join(format!("hop-{cp_txid}.rgbc"));
        let cosign = match cos {
            Some((job, addrs, _)) => {
                transfer.save_file(&tmp).map_err(|e| anyhow!("save consignment: {e}"))?;
                Some(Cosign { job, addrs, consignment: &tmp, spent: std::cell::Cell::new(Duration::ZERO) })
            }
            None => None,
        };
        let r = submit_hop(net, info, input, receivers, change, kps, Some(payload), cosign.as_ref()).await;
        if let (Some(c), Some((_, _, spent))) = (&cosign, cos) {
            spent.set(spent.get() + c.spent.get());
            let _ = std::fs::remove_file(&tmp);
        }
        let (c, a) = r?;
        ensure!(c == cp_txid, "checkpoint changed between the two builds");
        Ok(a)
    }
    .await;
    match res {
        Ok(a) => Ok((cp_txid, a, transfer)),
        Err(e) => {
            let rb = bl(|| rgb.ark_rollback_transfer(online.clone(), cp_txid.to_string()));
            println!("[rgb_hop] {cp_txid} not accepted ({e:#}); rollback: {:?}", rb.as_ref().map_err(|e| e.to_string()));
            Err(e)
        }
    }
}

/// #961 guard. arkd lets an unrolled VTXO join a batch as an on-chain input; for a colored output
/// the commitment tx would spend the seal without a commitment and the asset would be gone. The
/// delegate (and any wallet) refuses every batch input that is a known colored outpoint, on-chain
/// or not, and every on-chain input whose rgb-lib status says it carries state.
pub(crate) fn guard_batch_inputs(inputs: &[intent::Input], colored: &HashSet<OutPoint>, rgb: Option<(&RgbWallet, &Online)>) -> anyhow::Result<()> {
    for i in inputs {
        let op = i.outpoint();
        if colored.contains(&op) {
            bail!("guard (#961): {op} is a colored VTXO{}; it never joins a batch", if is_onchain(i) { " offered as an on-chain (boarding) input" } else { "" });
        }
        if let Some((w, online)) = rgb {
            if let Ok(st) = bl(|| w.ark_vtxo_status(online.clone(), op.to_string())) {
                bail!("guard (#961): {op} is tracked by the RGB wallet ({st:?}); it never joins a batch");
            }
        }
    }
    Ok(())
}

fn is_onchain(i: &intent::Input) -> bool {
    format!("{i:?}").contains("is_onchain: true")
}

/// Batch-refresh `inputs` into one VTXO at `to`, as the delegate: `client` holds only D (static
/// key provider) and joins the batch live, so it signs the intent and SIGHASH_DEFAULT forfeits
/// itself. (rust-sdk's presigned delegate flow uses ALL|ANYONECANPAY forfeits, which the arkd
/// signer refuses since v0.9.16, #58.) The #961 guard runs first.
pub(crate) async fn delegate_settle(
    client: &ArkClient,
    inputs: Vec<intent::Input>,
    to: ArkAddress,
    colored: &HashSet<OutPoint>,
    rgb: Option<(&RgbWallet, &Online)>,
) -> anyhow::Result<Txid> {
    guard_batch_inputs(&inputs, colored, rgb)?;
    let mut rng = <rand::rngs::StdRng as rand::SeedableRng>::from_entropy();
    let txid = tokio::time::timeout(Duration::from_secs(120), client.rgb_settle_inputs(&mut rng, inputs, to))
        .await
        .map_err(|_| anyhow!("batch timed out"))?
        .map_err(|e| anyhow!("batch: {e:#}"))?;
    Ok(txid)
}

pub(crate) async fn vtxo_at(net: &ark_grpc::Client, op: OutPoint) -> Option<VirtualTxOutPoint> {
    tokio::time::timeout(Duration::from_secs(10), net.list_vtxos(GetVtxosRequest::new_for_outpoints(&[op]))).await.ok()?.ok()?.vtxos.into_iter().next()
}

/// Wait (mining if needed) until the commitment txs of VTXO `op` are confirmed.
pub(crate) async fn wait_commitments(net: &ark_grpc::Client, regtest: &Regtest, op: OutPoint) -> anyhow::Result<()> {
    for i in 0..30 {
        let v = vtxo_at(net, op).await.ok_or_else(|| anyhow!("{op} unknown to the indexer"))?;
        if v.commitment_txids.iter().all(|t| regtest.tx_confirmations(t).is_some()) {
            return Ok(());
        }
        if i % 3 == 2 {
            crate::regtest::mine(1)?;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("commitment of {op} not confirmed")
}

/// Unspent VTXOs at `addrs`, in chunks.
pub(crate) async fn vtxos_at(net: &ark_grpc::Client, addrs: &[ArkAddress]) -> anyhow::Result<Vec<VirtualTxOutPoint>> {
    let mut out = vec![];
    for chunk in addrs.chunks(40) {
        let r = net.list_vtxos(GetVtxosRequest::new_for_addresses(chunk.iter().copied())).await.map_err(|e| anyhow!("list_vtxos: {e}"))?;
        out.extend(r.vtxos.into_iter().filter(|v| !v.is_spent && !v.is_swept));
    }
    Ok(out)
}

/// The VTXO a batch `commitment` created at `addr`.
pub(crate) async fn batch_output(net: &ark_grpc::Client, addr: ArkAddress, commitment: Txid) -> anyhow::Result<VirtualTxOutPoint> {
    for _ in 0..20 {
        if let Ok(v) = vtxos_at(net, &[addr]).await {
            if let Some(v) = v.into_iter().find(|v| v.commitment_txids.contains(&commitment) && !v.is_preconfirmed) {
                return Ok(v);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("no VTXO at {addr} from batch {commitment}")
}

/// Fully signed exit branch (commitment output -> VTXO) of any VTXO, from arkd's indexer.
pub(crate) async fn exit_branch(net: &ark_grpc::Client, regtest: &Regtest, v: OutPoint) -> anyhow::Result<Vec<Transaction>> {
    let info = vtxo_at(net, v).await.ok_or_else(|| anyhow!("{v} unknown to the indexer"))?;
    let chain = net.get_vtxo_chain(Some(v), None, None, None).await.map_err(|e| anyhow!("get_vtxo_chain: {e}"))?;
    let paths = ark_core::build_unilateral_exit_tree_txids(&chain.chains, v.txid)?;
    let all: Vec<String> = paths.concat().iter().collect::<HashSet<_>>().into_iter().map(|t| t.to_string()).collect();
    let txs = net.get_virtual_txs(all, None, None).await.map_err(|e| anyhow!("get_virtual_txs: {e}"))?.txs;
    let paths = paths
        .into_iter()
        .map(|p| p.into_iter().map(|t| txs.iter().find(|x| x.unsigned_tx.compute_txid() == t).cloned().ok_or_else(|| anyhow!("no PSBT for {t}"))).collect::<Result<Vec<_>, _>>())
        .collect::<Result<Vec<_>, _>>()?;
    let commitments = info.commitment_txids.iter().map(|t| regtest.raw_tx(t).ok_or_else(|| anyhow!("commitment {t} not found"))).collect::<Result<Vec<_>, _>>()?;
    let tree = ark_core::unilateral_exit::UnilateralExitTree::new(info.commitment_txids.clone(), paths);
    ark_core::unilateral_exit::finalize_unilateral_exit_tree(&tree, &commitments)?
        .into_iter()
        .find(|b| b.last().map(|t| t.compute_txid()) == Some(v.txid))
        .ok_or_else(|| anyhow!("no exit branch for {v}"))
}

pub(crate) const TOWER_RATES: [u64; 7] = [1, 2, 4, 8, 16, 32, 64];

/// Hand `branch` (exit of `vtxo`) to keyless watchtowers, with fee ladders presigned by `fee_sk`
/// (the payer's own P2WPKH key, funded here from the regtest faucet). Returns the registration size.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn register_towers(
    regtest: &Regtest,
    towers: &[PathBuf],
    id: &str,
    vtxo: OutPoint,
    expires_at: i64,
    threshold_s: i64,
    branch: &[Transaction],
    fee_sk: SecretKey,
) -> anyhow::Result<usize> {
    let fk = FeeKey::new(fee_sk)?;
    let addr = Address::from_script(&fk.spk, Network::Regtest)?;
    let mut fresh = vec![];
    for _ in 0..branch.len() {
        let txid = regtest.rpc("sendtoaddress", serde_json::json!([addr.to_string(), 0.0003]))?;
        fresh.push(Txid::from_str(txid.as_str().unwrap_or_default())?);
    }
    let mut utxos = vec![];
    for t in &fresh {
        let mut found = None;
        for _ in 0..20 {
            if let Some(tx) = regtest.raw_tx(t) {
                found = tx.output.iter().position(|o| o.script_pubkey == fk.spk).map(|i| (OutPoint { txid: *t, vout: i as u32 }, tx.output[i].value));
                if found.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        utxos.push(found.ok_or_else(|| anyhow!("fee utxo {t} not found"))?);
    }
    crate::regtest::mine(1)?;
    let ladders = branch.iter().zip(utxos.iter()).map(|(t, u)| fk.ladder(t, *u, &TOWER_RATES)).collect::<Result<Vec<_>, _>>()?;
    let reg = Registration {
        id: id.to_string(),
        vtxo: vtxo.to_string(),
        expires_at,
        threshold_s,
        branch: branch.iter().map(bitcoin::consensus::encode::serialize_hex).collect(),
        ladders,
    };
    let text = serde_json::to_string_pretty(&reg)?;
    for w in towers {
        std::fs::create_dir_all(registration_dir(w))?;
        std::fs::write(registration_dir(w).join(format!("{id}.json")), &text)?;
    }
    Ok(text.len())
}

/// Owner side: find the owner's live colored VTXOs by scanning the CLTV grid between `from` and
/// `to` (unix s). Needs only the template keys. Returns (VTXO, template) and the number of
/// addresses scanned.
pub(crate) async fn discover(net: &ark_grpc::Client, base: &Template, from: u64, to: u64) -> anyhow::Result<(Vec<(VirtualTxOutPoint, Template)>, usize)> {
    let mut cands = vec![];
    let mut t = from / T_GRID_S * T_GRID_S;
    while t <= to {
        cands.push(base.with_kind(Kind::Colored(t as u32)));
        t += T_GRID_S;
    }
    let addrs: Vec<ArkAddress> = cands.iter().map(|c| c.address()).collect();
    let found = vtxos_at(net, &addrs).await?;
    let by_spk: HashMap<ScriptBuf, Template> = cands.into_iter().map(|c| (c.spk(), c)).collect();
    let out = found.into_iter().filter_map(|v| by_spk.get(&v.script).cloned().map(|t| (v, t))).collect();
    Ok((out, addrs.len()))
}

/// What the delegate hands back to the owner after every renewal (no secrets).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub(crate) struct Handoff {
    pub asset_id: String,
    pub current: String,
    pub current_template: Option<Template>,
    pub consignment: String,
    pub proof: String,
    /// every template the asset sat on (for [`policy_check`])
    pub templates: Vec<Template>,
    /// unmined RGB witnesses of the history (checkpoints)
    pub witnesses: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct PolicyReport {
    pub witnesses: usize,
    pub delegate_spends: usize,
    pub violations: Vec<String>,
}

/// `<T> CLTV DROP <k> CHECKSIGVERIFY <server> CHECKSIG`?
/// (or `<T> CLTV DROP <k1> CHECKSIGVERIFY .. <kn> CHECKSIGVERIFY <server> CHECKSIG`, threshold)
pub(crate) fn delegate_style(script: &ScriptBuf, server: XOnlyPublicKey) -> bool {
    let ins: Vec<_> = script.instructions().filter_map(|i| i.ok()).collect();
    if ins.len() < 7 || (ins.len() - 3) % 2 != 0 {
        return false;
    }
    let head = matches!(&ins[..3], [Instruction::PushBytes(_), Instruction::Op(c), Instruction::Op(d)] if *c == OP_CLTV && *d == OP_DROP);
    let pairs: Vec<_> = ins[3..].chunks(2).collect();
    let last = pairs.len() - 1;
    head && pairs.iter().enumerate().all(|(i, p)| match p {
        [Instruction::PushBytes(k), Instruction::Op(o)] if i < last => k.len() == 32 && *o == OP_CHECKSIGVERIFY,
        [Instruction::PushBytes(s), Instruction::Op(o)] => s.as_bytes() == server.serialize() && *o == OP_CHECKSIG,
        _ => false,
    })
}

/// Receiver-side delegate rule over a consignment and its Ark proof: every witness that spends a
/// VTXO through a delegate-style leaf (`CLTV(T){x,S}`) must (1) spend a listed colored template
/// and (2) assign every revealed asset seal to a colored template with the same owner and
/// delegate. Concealed seals or unknown scripts fail closed.
pub(crate) fn policy_check(regtest: &Regtest, consignment: &Path, proof: &Path, templates: &[Template], server: XOnlyPublicKey) -> anyhow::Result<PolicyReport> {
    let t = RgbTransfer::load_file(consignment).map_err(|e| anyhow!("load consignment: {e}"))?;
    let pf: crate::proof::ProofFile = serde_json::from_str(&std::fs::read_to_string(proof)?)?;
    let psbts: HashMap<Txid, Psbt> = pf.psbts.iter().filter_map(|b| Psbt::from_str(b).ok()).map(|p| (p.unsigned_tx.compute_txid(), p)).collect();
    let spk_of = |op: OutPoint| -> Option<ScriptBuf> {
        if let Some(p) = psbts.get(&op.txid) {
            return p.unsigned_tx.output.get(op.vout as usize).map(|o| o.script_pubkey.clone());
        }
        regtest.raw_tx(&op.txid).and_then(|tx| tx.output.get(op.vout as usize).map(|o| o.script_pubkey.clone()))
    };
    let colored: Vec<&Template> = templates.iter().filter(|t| matches!(t.kind, Kind::Colored(_))).collect();
    let mut rep = PolicyReport::default();
    for wb in t.bundles.iter() {
        rep.witnesses += 1;
        let wid = Txid::from_str(&wb.witness_id().to_string())?;
        // (spending leaf, spent script) per input
        let spends: Vec<(ScriptBuf, Option<ScriptBuf>)> = if let Some(p) = psbts.get(&wid) {
            p.inputs
                .iter()
                .zip(&p.unsigned_tx.input)
                .filter_map(|(i, txin)| {
                    // the leaf from the PSBT fields, or from a finalized witness [.., script, control block]
                    let leaf = i.tap_scripts.values().next().map(|(s, _)| s.clone()).or_else(|| {
                        let w: Vec<&[u8]> = i.final_script_witness.as_ref()?.iter().collect();
                        (w.len() >= 2).then(|| ScriptBuf::from_bytes(w[w.len() - 2].to_vec()))
                    })?;
                    Some((leaf, i.witness_utxo.as_ref().map(|u| u.script_pubkey.clone()).or_else(|| spk_of(txin.previous_output))))
                })
                .collect()
        } else if let Some(tx) = regtest.raw_tx(&wid) {
            tx.input
                .iter()
                .filter_map(|i| {
                    let w: Vec<&[u8]> = i.witness.iter().collect();
                    (w.len() >= 2).then(|| (ScriptBuf::from_bytes(w[w.len() - 2].to_vec()), spk_of(i.previous_output)))
                })
                .collect()
        } else {
            rep.violations.push(format!("witness {wid} neither in the proof nor mined"));
            continue;
        };
        for (leaf, spent) in spends {
            if !delegate_style(&leaf, server) {
                continue;
            }
            rep.delegate_spends += 1;
            let Some(tpl) = colored.iter().find(|t| Some(t.spk()) == spent && t.delegate_leaves().contains(&leaf)) else {
                rep.violations.push(format!("{wid}: delegate-leaf spend of a VTXO that is no listed colored template"));
                continue;
            };
            for kt in wb.bundle.known_transitions.iter() {
                for (_, assigns) in kt.transition.assignments.iter() {
                    for a in assigns.as_fungible() {
                        let Some(seal) = a.revealed_seal() else {
                            rep.violations.push(format!("{wid}: concealed seal, cannot check the destination"));
                            continue;
                        };
                        let txid = seal.txid.to_string();
                        let Ok(txid) = Txid::from_str(&txid) else {
                            rep.violations.push(format!("{wid}: seal on the witness tx itself ({txid}:{}), not on an owner template", seal.vout));
                            continue;
                        };
                        let op = OutPoint { txid, vout: seal.vout.to_string().parse()? };
                        let ok = spk_of(op).is_some_and(|spk| colored.iter().any(|c| c.same_parties(tpl) && c.spk() == spk));
                        if !ok {
                            rep.violations.push(format!("{wid}: delegate moved the asset to {op}, not a template of owner {}", &tpl.owner[..16]));
                        }
                    }
                }
            }
        }
    }
    Ok(rep)
}

pub(crate) fn gz_len(p: &Path) -> u64 {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("gzip -9 -c '{}' | wc -c", p.display()))
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

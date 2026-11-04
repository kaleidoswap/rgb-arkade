//! emu-explore: RGB covenants enforced by the Arkade emulator (emulator:rgb) on arkd cponly.
//!
//! One generic covenant (tools/emucov outcome.go, `OutcomeCovenant(root)`): the covenant VTXO
//! has a fill leaf `<server> CHECKSIGVERIFY <emu tweaked by covenant> CHECKSIG` and an exit leaf
//! (CSV owner). The covenant pins a Merkle root of precommitted *outcomes*; each outcome (232-byte
//! leaf) fixes the two RGB commitments (C1 in the checkpoint, C2 at ark output 0) and the bitcoin
//! side (payment on output 1, RGB beneficiary on output 2, next covenant VTXO on output 3), an
//! emulator-clock window and an optional CSFS signer / oracle statement. See docs/emulator-capabilities.md.
//!
//! Every RGB transition is precomputed by the asset owner on placeholder txs (the commitments do
//! not depend on txids); the real txs carry the same OP_RETURNs and receivers re-anchor the
//! consignments to the real txids (`reanchor_all`).
use crate::offchain_transfer::{outs, wait_vtxo};
use crate::swap_emu::{EMU, add_prev_ark_txs, emu_last_error, emu_pubkey, emu_submit, emucov, emulator_up, spent};
use crate::swap_ni::{first, keypair_client, units};
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bare_opret, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_client::{ForfeitHookContext, GetVtxosRequestFilter};
use ark_core::ArkAddress;
use ark_core::script::{csv_sig_script, multisig_script};
use ark_core::send::{SendReceiver, VtxoInput, build_offchain_transactions};
use bitcoin::key::Secp256k1;
use bitcoin::script::Builder;
use bitcoin::{Amount, Network, OutPoint, Psbt, PublicKey, ScriptBuf, Transaction, TxIn, TxOut, Txid, XOnlyPublicKey, psbt};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::{Online, Wallet as RgbWallet};
use rgb_lib::{ContractId, FileContent, PubWitness, RgbTransfer};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const V_SAT: u64 = 2_000;
pub(crate) const CARRIER_SAT: u64 = 1_000;
pub(crate) const OPRET_SAT: u64 = 1;

/// One precommitted outcome (tools/emucov/outcome.go layout, 232 bytes).
#[derive(Clone, Default, Debug)]
pub(crate) struct Leaf {
    pub c1: [u8; 32],
    pub c2: [u8; 32],
    pub pay_key: [u8; 32],
    pub pay_min: u64,
    pub next_key: [u8; 32],
    pub next_min: u64,
    pub rgb_key: [u8; 32],
    pub auth_key: [u8; 32],
    pub not_before: u64,
    pub not_after: u64,
    pub price_below: u64,
}

impl Leaf {
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(232);
        b.extend(self.c1);
        b.extend(self.c2);
        b.extend(self.pay_key);
        b.extend(self.pay_min.to_le_bytes());
        b.extend(self.next_key);
        b.extend(self.next_min.to_le_bytes());
        b.extend(self.rgb_key);
        b.extend(self.auth_key);
        b.extend(self.not_before.to_le_bytes());
        b.extend(self.not_after.to_le_bytes());
        b.extend(self.price_below.to_le_bytes());
        b
    }
}

/// x-only output key of an Ark address (what the covenant compares on P2TR outputs).
pub(crate) fn out_key(a: &ArkAddress) -> [u8; 32] {
    let mut k = [0u8; 32];
    k.copy_from_slice(&a.to_p2tr_script_pubkey().as_bytes()[2..34]);
    k
}

/// A covenant VTXO. Tapleaves: fill (server + emulator tweaked by the outcome covenant), an
/// optional collaborative leaf (server + every party: renewal/migration without the emulator),
/// and the unilateral exits: one CSV N-of-N leaf per key set (arkd accepts only
/// CSVMultisig / ConditionCSVMultisig exit closures). Offers keep the single-owner exit.
pub(crate) struct CovVtxo {
    pub covenant: String,
    pub leaves: Vec<Leaf>,
    pub proofs: Vec<String>,
    pub fill: ScriptBuf,
    pub collab: Option<ScriptBuf>,
    pub exits: Vec<ScriptBuf>,
    pub vtxo: ark_core::Vtxo,
    pub addr: ArkAddress,
}

/// `<seq> CSV DROP <k1> CHECKSIGVERIFY ... <kn> CHECKSIG` (arkd CSVMultisigClosure layout).
pub(crate) fn csv_multisig_script(seq: bitcoin::Sequence, keys: &[XOnlyPublicKey]) -> ScriptBuf {
    use bitcoin::opcodes::all::{OP_CHECKSIG, OP_CHECKSIGVERIFY, OP_CSV, OP_DROP};
    let mut b = Builder::new().push_int(seq.to_consensus_u32() as i64).push_opcode(OP_CSV).push_opcode(OP_DROP);
    for (i, k) in keys.iter().enumerate() {
        b = b.push_x_only_key(k).push_opcode(if i + 1 == keys.len() { OP_CHECKSIG } else { OP_CHECKSIGVERIFY });
    }
    b.into_script()
}

/// `<k1> CHECKSIGVERIFY ... <kn> CHECKSIG` (arkd MultisigClosure, checksig type).
pub(crate) fn nofn_script(keys: &[XOnlyPublicKey]) -> ScriptBuf {
    use bitcoin::opcodes::all::{OP_CHECKSIG, OP_CHECKSIGVERIFY};
    let mut b = Builder::new();
    for (i, k) in keys.iter().enumerate() {
        b = b.push_x_only_key(k).push_opcode(if i + 1 == keys.len() { OP_CHECKSIG } else { OP_CHECKSIGVERIFY });
    }
    b.into_script()
}

impl CovVtxo {
    pub fn new(info: &ark_core::server::Info, emu: &PublicKey, owner: XOnlyPublicKey, leaves: Vec<Leaf>) -> anyhow::Result<Self> {
        Self::new_with(info, emu, owner, leaves, None)
    }

    /// `asset`: also require >= min units of this Arkade asset on output 1 (asset packet introspection).
    pub fn new_with(info: &ark_core::server::Info, emu: &PublicKey, owner: XOnlyPublicKey, leaves: Vec<Leaf>, asset: Option<(ark_core::asset::AssetId, u64)>) -> anyhow::Result<Self> {
        Self::new_full(info, emu, &[vec![owner]], None, leaves, asset)
    }

    /// Multi-party contract: `exits` = key sets of the CSV exit leaves (each N-of-N), `collab` =
    /// parties of the server-cosigned collaborative leaf.
    pub fn new_full(info: &ark_core::server::Info, emu: &PublicKey, exits: &[Vec<XOnlyPublicKey>], collab: Option<&[XOnlyPublicKey]>, leaves: Vec<Leaf>, asset: Option<(ark_core::asset::AssetId, u64)>) -> anyhow::Result<Self> {
        let hexes: Vec<String> = leaves.iter().map(|l| hex::encode(l.bytes())).collect();
        let mut args = vec!["tree"];
        args.extend(hexes.iter().map(|s| s.as_str()));
        let t = emucov(&args)?;
        let mut it = t.split(' ').map(|s| s.to_string());
        let root = it.next().ok_or_else(|| anyhow!("tree output"))?;
        let mut proofs: Vec<String> = it.collect();
        // a single-leaf tree has an empty proof, which the trimmed output drops
        if leaves.len() == 1 && proofs.is_empty() { proofs.push(String::new()); }
        ensure!(proofs.len() == leaves.len(), "tree: {} proofs for {} leaves", proofs.len(), leaves.len());
        let covenant = match asset {
            None => emucov(&["outcome-script", &root])?,
            Some((id, min)) => {
                use bitcoin::hashes::Hash;
                emucov(&["outcome-script", &root, &hex::encode(id.txid.to_byte_array()), &id.group_index.to_string(), &min.to_string()])?
            }
        };
        let tweaked = XOnlyPublicKey::from_str(&emucov(&["tweak", &emu.to_string(), &covenant])?)?;
        let server = info.signer_pk.x_only_public_key().0;
        let fill = multisig_script(server, tweaked);
        let collab = collab.map(|ks| nofn_script(&[&[server][..], ks].concat()));
        let owner = exits.first().and_then(|k| k.first()).copied().unwrap_or(server);
        let exits: Vec<ScriptBuf> = exits.iter().map(|ks| if ks.len() == 1 { csv_sig_script(info.unilateral_exit_delay, ks[0]) } else { csv_multisig_script(info.unilateral_exit_delay, ks) }).collect();
        let mut scripts = vec![fill.clone()];
        scripts.extend(collab.clone());
        scripts.extend(exits.clone());
        let vtxo = ark_core::Vtxo::new_with_custom_scripts(&Secp256k1::new(), server, owner, scripts, info.unilateral_exit_delay, Network::Regtest)?;
        let addr = vtxo.to_ark_address();
        Ok(Self { covenant, leaves, proofs, fill, collab, exits, vtxo, addr })
    }

    pub fn key(&self) -> [u8; 32] {
        out_key(&self.addr)
    }

    pub fn scripts(&self) -> Vec<ScriptBuf> {
        let mut v = vec![self.fill.clone()];
        v.extend(self.collab.clone());
        v.extend(self.exits.clone());
        v
    }

    pub fn input(&self, amount: u64, outpoint: OutPoint) -> anyhow::Result<VtxoInput> {
        self.input_leaf(&self.fill, amount, outpoint)
    }

    pub fn input_leaf(&self, leaf: &ScriptBuf, amount: u64, outpoint: OutPoint) -> anyhow::Result<VtxoInput> {
        let cb = self.vtxo.get_spend_info(leaf.clone())?;
        Ok(VtxoInput::new(leaf.clone(), None, cb, self.scripts(), self.addr.to_p2tr_script_pubkey(), Amount::from_sat(amount), outpoint, vec![]))
    }
}

/// Human-readable list of the exit leaves: keys per leaf (short) + script size.
pub(crate) fn exit_leaf_report(c: &CovVtxo) -> String {
    let leaves: Vec<String> = c.exits.iter().map(|e| {
        let keys: Vec<String> = e.instructions().filter_map(|i| match i { Ok(bitcoin::script::Instruction::PushBytes(b)) if b.len() == 32 => Some(hex::encode(&b.as_bytes()[..4])), _ => None }).collect();
        format!("CSV {}-of-{} [{}] {} B", keys.len(), keys.len(), keys.join(","), e.len())
    }).collect();
    format!("{} tapleaves: fill (server+emulator) {}{}; exits: {}", c.scripts().len(), if c.collab.is_some() { "+ collab; " } else { "" }, "", leaves.join(" | "))
}

/// A precomputed RGB transition: placeholder witness txid + commitment + consignment file.
#[derive(Clone)]
pub(crate) struct Pre {
    pub ph_txid: Txid,
    pub c: [u8; 32],
    pub consignment: PathBuf,
}

/// Placeholder PSBT: output 0 is the bare OP_RETURN host; inputs only carry outpoints.
pub(crate) fn placeholder(inputs: &[OutPoint], outs: usize, tag: u8) -> Psbt {
    let dummy = ScriptBuf::from_bytes([&[0x51u8, 0x20][..], &[tag; 32][..]].concat());
    let mut output = vec![bare_opret(Amount::ZERO)];
    for i in 1..outs {
        output.push(TxOut { value: Amount::from_sat(1_000 + i as u64), script_pubkey: dummy.clone() });
    }
    let tx = Transaction {
        version: bitcoin::transaction::Version(3),
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: inputs.iter().map(|o| TxIn { previous_output: *o, ..Default::default() }).collect(),
        output,
    };
    let mut p = Psbt::from_unsigned_tx(tx).unwrap();
    p.inputs = vec![psbt::Input::default(); inputs.len()];
    p
}

/// Colors `p` (moving all the asset found on its inputs) and saves the consignment.
pub(crate) fn precolor(rgb: &RgbWallet, p: &mut Psbt, cid: ContractId, bens: Vec<(SealTarget, u64, u64)>, file: PathBuf) -> anyhow::Result<Pre> {
    let t: RgbTransfer = bl(|| rgb.color_psbt_and_consume_custom(p, cid, bens))?;
    t.save_file(&file).map_err(|e| anyhow!("save: {e}"))?;
    let spk = &p.unsigned_tx.output[0].script_pubkey;
    ensure!(spk.is_op_return() && spk.len() == 34, "bad commitment {spk}");
    let mut c = [0u8; 32];
    c.copy_from_slice(&spk.as_bytes()[2..]);
    Ok(Pre { ph_txid: p.unsigned_tx.compute_txid(), c, consignment: file })
}

/// Re-anchors every bundle whose witness is a placeholder txid onto the real tx.
pub(crate) fn reanchor_all(src: &Path, dst: &Path, map: &[(Txid, Transaction)]) -> anyhow::Result<usize> {
    let mut c = RgbTransfer::load_file(src).map_err(|e| anyhow!("load consignment: {e:?}"))?;
    let mut n = 0;
    for wb in c.bundles.iter_mut() {
        if let Some((_, tx)) = map.iter().find(|(f, _)| wb.witness_id() == *f) {
            wb.pub_witness = PubWitness::with(tx.clone());
            n += 1;
        }
    }
    c.save_file(dst).map_err(|e| anyhow!("save consignment: {e}"))?;
    Ok(n)
}

/// A spend of a covenant VTXO (built by anyone; the emulator checks it).
pub(crate) struct CovSpend {
    pub psbt: Psbt,
    pub cps: Vec<Psbt>,
    pub txid: Txid,
    pub cp: Transaction,
    pub ext_bytes: usize,
}

/// Builds the ark tx spending covenant VTXO `v` with outcome `idx`: outputs
/// [OP_RETURN C2, mid..., change?, extension, anchor]; `payer` adds its VTXO as an extra input
/// (it funds `mid` and gets the change). `stmt`/`auth_sig` are the CSFS witness items.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cov_spend(
    info: &ark_core::server::Info,
    cov: &CovVtxo,
    v: OutPoint,
    v_sat: u64,
    idx: usize,
    c2_override: Option<[u8; 32]>,
    mid: Vec<TxOut>,
    payer: Option<(&ArkClient, OutPoint)>,
    change_addr: &ArkAddress,
    stmt: &[u8],
    auth_sig: &[u8],
) -> anyhow::Result<CovSpend> {
    cov_spend_x(info, cov, v, v_sat, idx, c2_override, mid, payer, change_addr, stmt, auth_sig, None).await
}

/// cov_spend + an Arkade asset transfer from the payer's input: `asset` = (id, payer's units,
/// units to output 1); the rest goes to the change output. The asset packet and the emulator
/// packet share the extension output.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cov_spend_x(
    info: &ark_core::server::Info,
    cov: &CovVtxo,
    v: OutPoint,
    v_sat: u64,
    idx: usize,
    c2_override: Option<[u8; 32]>,
    mid: Vec<TxOut>,
    payer: Option<(&ArkClient, OutPoint)>,
    change_addr: &ArkAddress,
    stmt: &[u8],
    auth_sig: &[u8],
    asset: Option<(ark_core::asset::AssetId, u64, u64)>,
) -> anyhow::Result<CovSpend> {
    let leaf = &cov.leaves[idx];
    let v_in = cov.input(v_sat, v)?.with_checkpoint_op_return(Some(leaf.c1));
    let (own, b_sat, b) = match payer {
        Some((cl, b)) => {
            let b_in = cl.resolve_selected_send_inputs(&[b]).await?.remove(0);
            let s = b_in.amount().to_sat();
            (build_offchain_transactions(&[SendReceiver::bitcoin(change_addr.clone(), b_in.amount())], change_addr, &[b_in, v_in], info)?, s, Some(b))
        }
        None => (build_offchain_transactions(&[SendReceiver::bitcoin(change_addr.clone(), Amount::from_sat(v_sat))], change_addr, &[v_in], info)?, 0, None),
    };
    let vin = own.ark_tx.unsigned_tx.input.iter().position(|i| own.checkpoint_txs.iter().any(|c| c.unsigned_tx.compute_txid() == i.previous_output.txid && c.unsigned_tx.input[0].previous_output == v)).ok_or_else(|| anyhow!("no V input"))?;
    let cp_v = own.checkpoint_txs.iter().find(|c| c.unsigned_tx.input[0].previous_output == v).unwrap().unsigned_tx.clone();
    let raw_cp = bitcoin::consensus::serialize(&cp_v);
    let anchor = own.ark_tx.unsigned_tx.output.iter().find(|o| o.value == Amount::ZERO && !o.script_pubkey.is_op_return()).cloned().ok_or_else(|| anyhow!("no anchor"))?;
    let mid_sum: u64 = mid.iter().map(|o| o.value.to_sat()).sum();
    let total = b_sat + v_sat;
    ensure!(total >= OPRET_SAT + mid_sum, "spend underfunded: {total} < {}", OPRET_SAT + mid_sum);
    let change = total - OPRET_SAT - mid_sum;
    let mut opret = bare_opret(Amount::from_sat(OPRET_SAT));
    opret.script_pubkey = ScriptBuf::new_op_return(c2_override.unwrap_or(leaf.c2));
    let w = format!("{},{},{},{},{}", hex::encode(stmt), hex::encode(auth_sig), cov.proofs[idx], hex::encode(leaf.bytes()), hex::encode(&raw_cp));
    let mut ext_args = vec!["ext".to_string(), vin.to_string(), cov.covenant.clone(), w];
    if let Some((id, total, price)) = asset {
        use ark_core::asset::packet::{AssetGroup, AssetInput, AssetOutput, Packet};
        ensure!(change > 0, "asset change needs a change output");
        let change_vout = (1 + mid.len()) as u16;
        // a zero amount is rejected by the packet parser, so omit the entry instead
        let mut outputs = vec![];
        if price > 0 { outputs.push(AssetOutput { output_index: 1, amount: price }); }
        if total > price {
            outputs.push(AssetOutput { output_index: change_vout, amount: total - price });
        }
        let g = AssetGroup { asset_id: Some(id), control_asset: None, metadata: None, inputs: vec![AssetInput { input_index: (1 - vin) as u16, amount: total }], outputs };
        ext_args.push(format!("0:{}", hex::encode(Packet { groups: vec![g] }.encode())));
    }
    let ext_refs: Vec<&str> = ext_args.iter().map(|x| x.as_str()).collect();
    let ext = ScriptBuf::from_bytes(hex::decode(emucov(&ext_refs)?)?);
    let ext_bytes = ext.len();
    let mut tx = own.ark_tx.unsigned_tx.clone();
    let mut o = vec![opret];
    o.extend(mid);
    if change > 0 {
        o.push(TxOut { value: Amount::from_sat(change), script_pubkey: change_addr.to_p2tr_script_pubkey() });
    }
    o.push(TxOut { value: Amount::ZERO, script_pubkey: ext });
    o.push(anchor);
    tx.output = o;
    let mut p = Psbt::from_unsigned_tx(tx)?;
    p.inputs = own.ark_tx.inputs.clone();
    for i in p.inputs.iter_mut() {
        i.sighash_type = None;
    }
    let mut cps = own.checkpoint_txs.clone();
    if let (Some((cl, _)), Some(b)) = (payer, b) {
        add_prev_ark_txs(cl, &mut p, &own.checkpoint_txs).await?;
        let bin = 1 - vin;
        cl.sign_ark_tx_inputs(&mut p, &[bin]).map_err(|e| anyhow!("{e:?}"))?;
        for c in cps.iter_mut() {
            if c.unsigned_tx.input[0].previous_output == b {
                if c.inputs[0].witness_script.is_none() {
                    c.inputs[0].witness_script = own.ark_tx.inputs[bin].witness_script.clone();
                }
                cl.sign_checkpoint_tx(c).map_err(|e| anyhow!("{e:?}"))?;
            }
        }
    }
    let txid = p.unsigned_tx.compute_txid();
    Ok(CovSpend { psbt: p, cps, txid, cp: cp_v, ext_bytes })
}

/// prevarktx fields for spends without a payer client (any client can read virtual txs).
pub(crate) async fn with_prev(client: &ArkClient, s: &mut CovSpend) -> anyhow::Result<()> {
    if !s.psbt.inputs.iter().all(|i| i.unknown.keys().any(|k| k.type_value == 0xde && k.key == b"prevarktx")) {
        add_prev_ark_txs(client, &mut s.psbt, &s.cps).await?;
    }
    Ok(())
}

pub(crate) fn p2tr(k: [u8; 32]) -> ScriptBuf {
    ScriptBuf::from_bytes([&[0x51u8, 0x20][..], &k[..]].concat())
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

// ---------------------------------------------------------------------------------------------
// Scenario 1: emu-lots. Offer of ISSUE_AMOUNT units in N fixed lots, with an emulator-clock
// expiry. Each lot = one outcome of the current offer VTXO V_k: pay PRICE to the maker, lot to the
// taker's vout 2, the rest to V_{k-1} (vout 3, a new covenant VTXO whose address is pinned). After
// the expiry the maker reclaims the rest (rgbKey = maker, notBefore = expiry).
// ---------------------------------------------------------------------------------------------
const LOT: u64 = 300;
const PRICE_SAT: u64 = 20_000;

struct Level {
    k: usize, // lots left
    t1: Pre,
    fill: Pre,
    rec: Pre,
    fill_units: u64,
    rest: u64,
}

pub async fn run_lots() -> anyhow::Result<()> {
    let run_id = format!("run-{}-emu-lots", now());
    let state = crate::paths::state().join(&run_id);
    std::fs::create_dir_all(&state)?;
    println!("run id {run_id}, state dir {}", state.display());
    let mut summary = serde_json::Map::new();
    let mut rec = |k: &str, v: String| {
        println!("  >> {k} = {v}");
        summary.insert(k.to_string(), serde_json::Value::String(v));
        std::fs::write(state.join("summary.json"), serde_json::to_string_pretty(&summary).unwrap()).unwrap();
    };
    let regtest = Arc::new(Regtest::new());
    let expiry_s: u64 = std::env::var("EMU_LOTS_EXPIRY_S").ok().and_then(|s| s.parse().ok()).unwrap_or(480);
    let n_lots = ISSUE_AMOUNT.div_ceil(LOT) as usize; // 300, 300, 300, 100 -> 4 levels

    step("0. emulator:rgb next to arkd cponly; clients");
    emulator_up("emulator:rgb", &[])?;
    let emu = emu_pubkey().await?;
    let (alice, akp) = keypair_client("alice", regtest.clone(), &state).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    let (carol, _) = ark_client("carol", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    board("alice", &alice, "0.001").await?;
    board("bob", &bob, "0.001").await?;
    board("carol", &carol, "0.001").await?;
    let (a0, b0, c0) = (first(&alice, "alice").await?, first(&bob, "bob").await?, first(&carol, "carol").await?);
    let (alice_addr, _) = alice.get_offchain_address().await?;
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (carol_addr, _) = carol.get_offchain_address().await?;
    let maker = akp.x_only_public_key().0;
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;
    let (carol_rgb, carol_online) = bl(|| rgb_wallet("carol", &state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, a0, &state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    rec("asset_on_a", format!("{asset_id} on {a0}"));

    step("1. Alice precomputes every transition on placeholders (funding, then per level T1, T2 fill, T2 reclaim)");
    let t_pre = Instant::now();
    // funding: T1a in A's checkpoint (A -> CP:1), T2f in the funding ark tx (CP:1 -> V_N at vout 1)
    let a_in = alice.resolve_selected_send_inputs(&[a0]).await?.remove(0);
    let a_sat = a_in.amount().to_sat();
    let dry = build_offchain_transactions(&[SendReceiver::bitcoin(alice_addr.clone(), Amount::from_sat(V_SAT))], &alice_addr, &[a_in.clone().with_checkpoint_op_return(Some([0u8; 32]))], &info)?;
    let mut cp_a = dry.checkpoint_txs[0].clone();
    cp_a.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    let t1a = precolor(&alice_rgb, &mut cp_a, cid, vec![(SealTarget::WitnessVout(1), ISSUE_AMOUNT, 9200)], state.join("t1a.rgbc"))?;
    let fund_ph = placeholder(&[OutPoint { txid: t1a.ph_txid, vout: 1 }], 3, 0xf0);
    let mut fund_ph = fund_ph;
    let t2f = precolor(&alice_rgb, &mut fund_ph, cid, vec![(SealTarget::WitnessVout(1), ISSUE_AMOUNT, 9201)], state.join("t2f.rgbc"))?;
    let mut levels: Vec<Level> = vec![];
    let mut v_ph = OutPoint { txid: t2f.ph_txid, vout: 1 };
    let mut rest = ISSUE_AMOUNT;
    for i in 0..n_lots {
        let k = n_lots - i;
        let mut cp = placeholder(&[v_ph], 3, 0xc0 + i as u8);
        let t1 = precolor(&alice_rgb, &mut cp, cid, vec![(SealTarget::WitnessVout(1), rest, 9300 + i as u64)], state.join(format!("t1-{k}.rgbc")))?;
        let cp1 = OutPoint { txid: t1.ph_txid, vout: 1 };
        let mut rp = placeholder(&[OutPoint::null(), cp1], 3, 0xa0 + i as u8);
        let rec_t = precolor(&alice_rgb, &mut rp, cid, vec![(SealTarget::WitnessVout(2), rest, 9400 + i as u64)], state.join(format!("rec-{k}.rgbc")))?;
        let lot = rest.min(LOT);
        let mut fp = placeholder(&[OutPoint::null(), cp1], if lot < rest { 4 } else { 3 }, 0xb0 + i as u8);
        let mut bens = vec![(SealTarget::WitnessVout(2), lot, 9500 + i as u64)];
        if lot < rest {
            bens.push((SealTarget::WitnessVout(3), rest - lot, 9600 + i as u64));
        }
        let fill = precolor(&alice_rgb, &mut fp, cid, bens, state.join(format!("fill-{k}.rgbc")))
            .map_err(|e| anyhow!("T2 fill after T2 reclaim on the same allocation (level {k}): {e}"))?;
        levels.push(Level { k, t1, fill: fill.clone(), rec: rec_t, fill_units: lot, rest });
        v_ph = OutPoint { txid: fill.ph_txid, vout: 3 };
        rest -= lot;
    }
    // covenants bottom-up: V_1 has no next; V_k's fill pins V_{k-1}'s output key
    let expiry = unix_now() + expiry_s;
    let maker_key = out_key(&alice_addr);
    let mut covs: Vec<CovVtxo> = vec![];
    for l in levels.iter().rev() {
        let next_key = covs.last().map(|c: &CovVtxo| c.key()).unwrap_or([0u8; 32]);
        let fill = Leaf { c1: l.t1.c, c2: l.fill.c, pay_key: maker_key, pay_min: PRICE_SAT, next_key, next_min: if l.k > 1 { V_SAT } else { 0 }, not_after: expiry, ..Default::default() };
        let reclaim = Leaf { c1: l.t1.c, c2: l.rec.c, rgb_key: maker_key, not_before: expiry, ..Default::default() };
        covs.push(CovVtxo::new(&info, &emu, maker, vec![fill, reclaim])?);
    }
    covs.reverse(); // covs[i] <-> levels[i]
    rec("precompute_ms", t_pre.elapsed().as_millis().to_string());
    rec("levels", levels.iter().map(|l| format!("V{}: lot {} of {}", l.k, l.fill_units, l.rest)).collect::<Vec<_>>().join(", "));
    rec("covenant_bytes", format!("{} (same script shape for every level; root differs)", covs[0].covenant.len() / 2));
    rec("expiry", format!("{expiry} (emulator clock, now+{expiry_s}s)"));
    let pkg: usize = levels.iter().map(|l| std::fs::metadata(&l.fill.consignment).map(|m| m.len()).unwrap_or(0) as usize).max().unwrap_or(0);
    rec("offer_package", format!("{} leaves x 232 B + {} proofs x 32 B; largest precomputed consignment {pkg} B", covs.len() * 2, covs.len() * 2));

    step("2. Alice funds V_N in one ark tx (asset moves in with a witness-vout seal) and goes offline");
    let t_fund = Instant::now();
    let built = build_offchain_transactions(&[SendReceiver::bitcoin(covs[0].addr.clone(), Amount::from_sat(V_SAT))], &alice_addr, &[a_in.with_checkpoint_op_return(Some(t1a.c))], &info)?;
    let mut ark = built.ark_tx.clone();
    // arkd cponly: every output >= 1 sat; the 1 sat comes out of Alice's change (output after V_N)
    ensure!(ark.unsigned_tx.output.len() >= 2 && ark.unsigned_tx.output[1].script_pubkey == alice_addr.to_p2tr_script_pubkey(), "fund layout {:?}", outs(&ark));
    ark.unsigned_tx.output[1].value -= Amount::from_sat(OPRET_SAT);
    ark.unsigned_tx.output.insert(0, TxOut { value: Amount::from_sat(OPRET_SAT), script_pubkey: ScriptBuf::new_op_return(t2f.c) });
    ark.outputs.insert(0, psbt::Output::default());
    ensure!(ark.unsigned_tx.output[1].script_pubkey == covs[0].addr.to_p2tr_script_pubkey(), "V_N not at vout 1: {:?}", outs(&ark));
    let fund_tx = ark.unsigned_tx.clone();
    let cp_a_real = built.checkpoint_txs[0].unsigned_tx.clone();
    ensure!(cp_a_real.compute_txid() == t1a.ph_txid, "A's checkpoint differs from the precolored one");
    let pend = alice.submit_built_offchain_send(ark, built.checkpoint_txs, maker).await.map_err(|e| anyhow!("fund: {e:?}"))?;
    alice.sign_and_finalize_pending_tx(pend).await.map_err(|e| anyhow!("fund finalize: {e:?}"))?;
    let mut v = OutPoint { txid: fund_tx.compute_txid(), vout: 1 };
    rec("fund", format!("{} ms; V_N {v} ({a_sat} sat input)", t_fund.elapsed().as_millis()));
    let mut map: Vec<(Txid, Transaction)> = vec![(t2f.ph_txid, fund_tx.clone())];
    let mut offchain: Vec<String> = vec![cp_a_real.compute_txid().to_string(), fund_tx.compute_txid().to_string()];

    // takers fill levels 0 and 1; negatives on level 0 first
    let takers: [(&str, &ArkClient, &RgbWallet, &Online, OutPoint, &ArkAddress); 2] =
        [("bob", &bob, &bob_rgb, &bob_online, b0, &bob_addr), ("carol", &carol, &carol_rgb, &carol_online, c0, &carol_addr)];
    let lot_outs = |cov_next: Option<&CovVtxo>, buyer: &ArkAddress, pay: u64, next_spk: Option<ScriptBuf>, next_sat: u64| {
        let mut m = vec![
            TxOut { value: Amount::from_sat(pay), script_pubkey: alice_addr.to_p2tr_script_pubkey() },
            TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: buyer.to_p2tr_script_pubkey() },
        ];
        if let Some(c) = cov_next {
            m.push(TxOut { value: Amount::from_sat(next_sat), script_pubkey: next_spk.unwrap_or_else(|| c.addr.to_p2tr_script_pubkey()) });
        }
        m
    };
    let mut changes: Vec<OutPoint> = vec![];
    for (i, (name, cl, rgbw, online, b, addr)) in takers.iter().enumerate() {
        let lvl = &levels[i];
        let cov = &covs[i];
        let next = covs.get(i + 1);
        step(&format!("3.{i} {name} fills lot {} of V{} via the emulator", i + 1, lvl.k));
        if i == 0 {
            for (neg, pay, nspk, nsat, idx) in [
                ("underpay", PRICE_SAT - 1, None, V_SAT, 0usize),
                ("remainder_redirected", PRICE_SAT, Some(addr.to_p2tr_script_pubkey()), V_SAT, 0),
                ("remainder_sats_drained", PRICE_SAT, None, V_SAT - 1, 0),
                ("reclaim_before_expiry_by_taker", PRICE_SAT, None, V_SAT, 1),
            ] {
                let s = cov_spend(&info, cov, v, V_SAT, idx, None, lot_outs(next, addr, pay, nspk, nsat), Some((cl, *b)), addr, &[], &[]).await?;
                let r = emu_submit(&s.psbt, &s.cps).await;
                rec(&format!("neg_{neg}"), format!("{:?} / {}", r.as_ref().err().map(|e| e.chars().take(80).collect::<String>()), emu_last_error()));
                ensure!(r.is_err(), "emulator accepted {neg}");
            }
            rec("after_negatives", format!("V {}; B {}", spent(v).await, spent(*b).await));
        }
        let t = Instant::now();
        let s = cov_spend(&info, cov, v, V_SAT, 0, None, lot_outs(next, addr, PRICE_SAT, None, V_SAT), Some((cl, *b)), addr, &[], &[]).await?;
        let signed = emu_submit(&s.psbt, &s.cps).await.map_err(|e| anyhow!("emulator: {e} / {}", emu_last_error()))?;
        ensure!(signed.unsigned_tx.compute_txid() == s.txid, "emulator returned another tx");
        let lot_v = OutPoint { txid: s.txid, vout: 2 };
        wait_vtxo(cl, lot_v, name).await?;
        rec(&format!("fill{}_ms", i + 1), t.elapsed().as_millis().to_string());
        rec(&format!("fill{}", i + 1), format!("ark tx {} ({} vB, extension {} B): {:?}", s.txid, s.psbt.unsigned_tx.vsize(), s.ext_bytes, outs(&s.psbt).iter().map(|o| o.0).collect::<Vec<_>>()));
        // receiver: re-anchor the precomputed consignment to the real txs and validate offline
        map.push((lvl.t1.ph_txid, s.cp.clone()));
        map.push((lvl.fill.ph_txid, s.psbt.unsigned_tx.clone()));
        offchain.push(s.cp.compute_txid().to_string());
        offchain.push(s.txid.to_string());
        let fin = state.join(format!("lot{}-{name}.rgbc", i + 1));
        let n = reanchor_all(&lvl.fill.consignment, &fin, &map)?;
        let tv = Instant::now();
        let got = bl(|| rgbw.validate_consignment_offchain((*online).clone(), fin.clone(), offchain.clone(), s.txid.to_string(), 2))?;
        rec(&format!("lot{}_{name}_valid_units", i + 1), format!("{} (re-anchored {n} bundles, consignment {} B, validate {} ms)", units(&got), std::fs::metadata(&fin)?.len(), tv.elapsed().as_millis()));
        ensure!(units(&got) == lvl.fill_units, "{name} gets {}", units(&got));
        if next.is_some() {
            let rest_v = bl(|| rgbw.validate_consignment_offchain((*online).clone(), fin.clone(), offchain.clone(), s.txid.to_string(), 3))?;
            rec(&format!("lot{}_rest_on_next_offer", i + 1), units(&rest_v).to_string());
            ensure!(units(&rest_v) == lvl.rest - lvl.fill_units, "rest on V_(k-1) {}", units(&rest_v));
        }
        let pay_v = OutPoint { txid: s.txid, vout: 1 };
        wait_vtxo(&alice, pay_v, "alice").await?;
        rec(&format!("lot{}_maker_paid", i + 1), format!("{pay_v}"));
        v = OutPoint { txid: s.txid, vout: 3 };
        changes.push(OutPoint { txid: s.txid, vout: 4 });
    }
    let lvl_i = takers.len();
    let lvl = &levels[lvl_i];
    let cov = &covs[lvl_i];

    step("4. before the expiry: the maker's reclaim is refused");
    let reclaim_outs = || vec![
        TxOut { value: Amount::from_sat(V_SAT - OPRET_SAT - CARRIER_SAT), script_pubkey: alice_addr.to_p2tr_script_pubkey() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: alice_addr.to_p2tr_script_pubkey() },
    ];
    let mut s = cov_spend(&info, cov, v, V_SAT, 1, None, reclaim_outs(), None, &alice_addr, &[], &[]).await?;
    with_prev(&alice, &mut s).await?;
    let r = emu_submit(&s.psbt, &s.cps).await;
    rec("neg_reclaim_before_expiry", format!("{:?} / {}", r.as_ref().err().map(|e| e.chars().take(80).collect::<String>()), emu_last_error()));
    ensure!(r.is_err(), "reclaim accepted before the expiry");

    step("5. wait for the expiry (emulator clock), then: a late fill is refused, the maker reclaims");
    let wait = expiry.saturating_sub(unix_now()) + 3;
    rec("waited_for_expiry_s", wait.to_string());
    tokio::time::sleep(Duration::from_secs(wait)).await;
    let b1 = changes[0]; // Bob's BTC change of fill 1
    let next = covs.get(lvl_i + 1);
    let late = cov_spend(&info, cov, v, V_SAT, 0, None, lot_outs(next, &bob_addr, PRICE_SAT, None, V_SAT), Some((&bob, b1)), &bob_addr, &[], &[]).await?;
    let r = emu_submit(&late.psbt, &late.cps).await;
    rec("neg_fill_after_expiry", format!("{:?} / {}", r.as_ref().err().map(|e| e.chars().take(80).collect::<String>()), emu_last_error()));
    ensure!(r.is_err(), "late fill accepted");
    let t = Instant::now();
    let signed = emu_submit(&s.psbt, &s.cps).await.map_err(|e| anyhow!("reclaim: {e} / {}", emu_last_error()))?;
    ensure!(signed.unsigned_tx.compute_txid() == s.txid, "emulator returned another tx");
    wait_vtxo(&alice, OutPoint { txid: s.txid, vout: 2 }, "alice").await?;
    rec("reclaim_ms", t.elapsed().as_millis().to_string());
    map.push((lvl.t1.ph_txid, s.cp.clone()));
    map.push((lvl.rec.ph_txid, s.psbt.unsigned_tx.clone()));
    offchain.push(s.cp.compute_txid().to_string());
    offchain.push(s.txid.to_string());
    let fin = state.join("reclaim.rgbc");
    let n = reanchor_all(&lvl.rec.consignment, &fin, &map)?;
    let got = bl(|| alice_rgb.validate_consignment_offchain(alice_online.clone(), fin.clone(), offchain.clone(), s.txid.to_string(), 2))?;
    rec("reclaim_valid_units", format!("{} (re-anchored {n} bundles, consignment {} B)", units(&got), std::fs::metadata(&fin)?.len()));
    ensure!(units(&got) == lvl.rest, "reclaim gives {}", units(&got));
    rec("v_after_reclaim", spent(v).await);
    let _ = (EMU, GetVtxosRequestFilter::Spendable, Builder::new());
    println!("EMU-LOTS RESULT: PASS");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Single-level contracts (escrow, lending, vault): the owner's asset on A is split into several
// covenant VTXOs created by ONE funding ark tx (witness-vout seals 1..n). Per contract: T1
// (V -> CP:1) and one T2 per outcome (CP:1 -> the outcome's vout).
// ---------------------------------------------------------------------------------------------

pub(crate) struct PreContract {
    pub t1: Pre,
    pub t2: Vec<Pre>,
}

pub(crate) struct Precomputed {
    pub t1a: Pre,
    pub t2f: Pre,
    pub a_in: VtxoInput,
    pub contracts: Vec<PreContract>,
}

/// `specs[i]` = (units, per outcome the T2 seals [(vout, units)]).
pub(crate) async fn precompute(owner: &ArkClient, info: &ark_core::server::Info, rgb: &RgbWallet, cid: ContractId, a0: OutPoint, change: &ArkAddress, specs: &[(u64, Vec<Vec<(u32, u64)>>)], state: &Path, tag: &str) -> anyhow::Result<Precomputed> {
    let a_in = owner.resolve_selected_send_inputs(&[a0]).await?.remove(0);
    precompute_in(info, rgb, cid, a_in, change, specs, state, tag)
}

/// precompute() from an explicit input (e.g. a multi-party VTXO spent through its collab leaf).
#[allow(clippy::too_many_arguments)]
pub(crate) fn precompute_in(info: &ark_core::server::Info, rgb: &RgbWallet, cid: ContractId, a_in: VtxoInput, change: &ArkAddress, specs: &[(u64, Vec<Vec<(u32, u64)>>)], state: &Path, tag: &str) -> anyhow::Result<Precomputed> {
    let recv: Vec<SendReceiver> = specs.iter().map(|_| SendReceiver::bitcoin(change.clone(), Amount::from_sat(V_SAT))).collect();
    let dry = build_offchain_transactions(&recv, change, &[a_in.clone().with_checkpoint_op_return(Some([0u8; 32]))], info)?;
    let mut cp_a = dry.checkpoint_txs[0].clone();
    cp_a.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    let total: u64 = specs.iter().map(|s| s.0).sum();
    let t1a = precolor(rgb, &mut cp_a, cid, vec![(SealTarget::WitnessVout(1), total, 9700)], state.join(format!("{tag}-t1a.rgbc")))?;
    let mut fp = placeholder(&[OutPoint { txid: t1a.ph_txid, vout: 1 }], specs.len() + 1, 0xf1);
    let bens = specs.iter().enumerate().map(|(i, s)| (SealTarget::WitnessVout(1 + i as u32), s.0, 9710 + i as u64)).collect();
    let t2f = precolor(rgb, &mut fp, cid, bens, state.join(format!("{tag}-t2f.rgbc")))?;
    let mut contracts = vec![];
    for (i, (units, outcomes)) in specs.iter().enumerate() {
        let v_ph = OutPoint { txid: t2f.ph_txid, vout: 1 + i as u32 };
        let mut cp = placeholder(&[v_ph], 3, 0xd0 + i as u8);
        let t1 = precolor(rgb, &mut cp, cid, vec![(SealTarget::WitnessVout(1), *units, 9720 + i as u64)], state.join(format!("{tag}-c{i}-t1.rgbc")))?;
        let mut t2 = vec![];
        for (j, seals) in outcomes.iter().enumerate() {
            let maxv = seals.iter().map(|s| s.0).max().unwrap_or(2) as usize;
            let mut p = placeholder(&[OutPoint::null(), OutPoint { txid: t1.ph_txid, vout: 1 }], maxv + 1, (0x10 * i as u8).wrapping_add(j as u8));
            let bens = seals.iter().enumerate().map(|(n, (v, u))| (SealTarget::WitnessVout(*v), *u, 9800 + 100 * i as u64 + 10 * j as u64 + n as u64)).collect();
            t2.push(precolor(rgb, &mut p, cid, bens, state.join(format!("{tag}-c{i}-o{j}.rgbc")))?);
        }
        contracts.push(PreContract { t1, t2 });
    }
    Ok(Precomputed { t1a, t2f, a_in, contracts })
}

/// The real funding ark tx: [OP_RETURN T2f, cov_0 .. cov_n-1, change, anchor]; returns (tx, cp of A).
pub(crate) async fn fund_contracts(owner: &ArkClient, owner_pk: XOnlyPublicKey, info: &ark_core::server::Info, pre: &Precomputed, covs: &[&CovVtxo], change: &ArkAddress) -> anyhow::Result<(Transaction, Transaction)> {
    let recv: Vec<SendReceiver> = covs.iter().map(|c| SendReceiver::bitcoin(c.addr.clone(), Amount::from_sat(V_SAT))).collect();
    let built = build_offchain_transactions(&recv, change, &[pre.a_in.clone().with_checkpoint_op_return(Some(pre.t1a.c))], info)?;
    let mut ark = built.ark_tx.clone();
    let n = covs.len();
    ensure!(ark.unsigned_tx.output.len() > n && ark.unsigned_tx.output[n].script_pubkey == change.to_p2tr_script_pubkey(), "fund layout {:?}", outs(&ark));
    ark.unsigned_tx.output[n].value -= Amount::from_sat(OPRET_SAT);
    ark.unsigned_tx.output.insert(0, TxOut { value: Amount::from_sat(OPRET_SAT), script_pubkey: ScriptBuf::new_op_return(pre.t2f.c) });
    ark.outputs.insert(0, psbt::Output::default());
    let cp_a = built.checkpoint_txs[0].unsigned_tx.clone();
    ensure!(cp_a.compute_txid() == pre.t1a.ph_txid, "A's checkpoint differs from the precolored one");
    let tx = ark.unsigned_tx.clone();
    let pend = owner.submit_built_offchain_send(ark, built.checkpoint_txs, owner_pk).await.map_err(|e| anyhow!("fund: {e:?}"))?;
    owner.sign_and_finalize_pending_tx(pend).await.map_err(|e| anyhow!("fund finalize: {e:?}"))?;
    Ok((tx, cp_a))
}

pub(crate) fn auth_sig(kp: &bitcoin::key::Keypair, leaf: &Leaf, v: OutPoint) -> Vec<u8> {
    use bitcoin::hashes::{Hash, sha256};
    let msg = sha256::Hash::hash(&[&leaf.bytes()[..], &bitcoin::consensus::serialize(&v)[..]].concat());
    Secp256k1::new().sign_schnorr_no_aux_rand(&bitcoin::secp256k1::Message::from_digest(msg.to_byte_array()), kp).serialize().to_vec()
}

pub(crate) fn oracle_stmt(kp: &bitcoin::key::Keypair, price: u64, ts: u64) -> (Vec<u8>, Vec<u8>) {
    use bitcoin::hashes::{Hash, sha256};
    let stmt = [price.to_le_bytes(), ts.to_le_bytes()].concat();
    let msg = sha256::Hash::hash(&stmt);
    let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&bitcoin::secp256k1::Message::from_digest(msg.to_byte_array()), kp).serialize().to_vec();
    (stmt, sig)
}

pub(crate) fn new_keypair() -> bitcoin::key::Keypair {
    bitcoin::key::Keypair::new(&Secp256k1::new(), &mut rand::thread_rng())
}

/// Context shared by the single-level scenarios.
pub(crate) struct Ctx {
    pub state: PathBuf,
    pub map: Vec<(Txid, Transaction)>,
    pub offchain: Vec<String>,
}

impl Ctx {
    /// Submits a covenant spend; on success records the real txs and returns (consignment, txid).
    pub async fn settle(&mut self, any: &ArkClient, mut s: CovSpend, cov_t1: &Pre, t2: &Pre, name: &str) -> anyhow::Result<(PathBuf, Txid, u128)> {
        with_prev(any, &mut s).await?;
        let t = Instant::now();
        let signed = emu_submit(&s.psbt, &s.cps).await.map_err(|e| anyhow!("{name}: {e} / {}", emu_last_error()))?;
        let ms = t.elapsed().as_millis();
        ensure!(signed.unsigned_tx.compute_txid() == s.txid, "emulator returned another tx");
        self.map.push((cov_t1.ph_txid, s.cp.clone()));
        self.map.push((t2.ph_txid, s.psbt.unsigned_tx.clone()));
        self.offchain.push(s.cp.compute_txid().to_string());
        self.offchain.push(s.txid.to_string());
        let fin = self.state.join(format!("{name}.rgbc"));
        reanchor_all(&t2.consignment, &fin, &self.map)?;
        Ok((fin, s.txid, ms))
    }

    /// Must be refused by the emulator; returns the emulator's reason.
    pub async fn refuse(&self, any: &ArkClient, mut s: CovSpend) -> anyhow::Result<String> {
        with_prev(any, &mut s).await?;
        let r = emu_submit(&s.psbt, &s.cps).await;
        ensure!(r.is_err(), "emulator accepted a spend that must be refused");
        Ok(emu_last_error())
    }
}

pub(crate) fn valid_units(w: &RgbWallet, online: &Online, fin: &Path, offchain: &[String], txid: Txid, vout: u32) -> anyhow::Result<u64> {
    let got = bl(|| w.validate_consignment_offchain(online.clone(), fin.to_path_buf(), offchain.to_vec(), txid.to_string(), vout))?;
    Ok(units(&got))
}

/// Outputs [out1 (btc, to `btc_to`), out2 carrier (to `rgb_to`)] for spends funded by V alone.
pub(crate) fn plain_outs(btc_to: &ArkAddress, rgb_to: &ArkAddress) -> Vec<TxOut> {
    vec![
        TxOut { value: Amount::from_sat(V_SAT - OPRET_SAT - CARRIER_SAT), script_pubkey: btc_to.to_p2tr_script_pubkey() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: rgb_to.to_p2tr_script_pubkey() },
    ]
}

struct Scn {
    state: PathBuf,
    summary: serde_json::Map<String, serde_json::Value>,
}

impl Scn {
    fn new(name: &str) -> anyhow::Result<Self> {
        let run_id = format!("run-{}-{name}", now());
        let state = crate::paths::state().join(&run_id);
        std::fs::create_dir_all(&state)?;
        println!("run id {run_id}, state dir {}", state.display());
        Ok(Self { state, summary: serde_json::Map::new() })
    }
    fn rec(&mut self, k: &str, v: String) {
        println!("  >> {k} = {v}");
        self.summary.insert(k.to_string(), serde_json::Value::String(v));
        std::fs::write(self.state.join("summary.json"), serde_json::to_string_pretty(&self.summary).unwrap()).unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario 3: emu-escrow. Bob (buyer) locks 1000 units as payment for Sally (seller), arbiter
// Ari. Two escrow VTXOs from one funding tx: E1 (600) and E2 (400). Outcomes per escrow (RGB
// beneficiary pinned on output 2 in every outcome):
//   0 release -> Sally, signed by Bob     1 release -> Sally, signed by Ari
//   2 refund  -> Bob,  signed by Sally    3 refund  -> Bob,  signed by Ari
//   4 refund  -> Bob,  after the timeout (no signature)
// ---------------------------------------------------------------------------------------------
pub async fn run_escrow() -> anyhow::Result<()> {
    let mut sc = Scn::new("emu-escrow")?;
    let regtest = Arc::new(Regtest::new());
    let timeout_s: u64 = std::env::var("EMU_ESCROW_TIMEOUT_S").ok().and_then(|s| s.parse().ok()).unwrap_or(150);
    step("0. emulator, clients (Bob buyer with the asset, Sally seller, keys: Ari arbiter)");
    emulator_up("emulator:rgb", &[])?;
    let emu = emu_pubkey().await?;
    let (bob, bkp) = keypair_client("bob", regtest.clone(), &sc.state).await?;
    let (sally, _) = ark_client("sally", regtest.clone(), &sc.state).await?;
    let info = bob.server_info().await?;
    board("bob", &bob, "0.001").await?;
    let b0 = first(&bob, "bob").await?;
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (sally_addr, _) = sally.get_offchain_address().await?;
    let sally_kp = new_keypair();
    let ari = new_keypair();
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &sc.state))?;
    let (sally_rgb, sally_online) = bl(|| rgb_wallet("sally", &sc.state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut bob_rgb, &bob_online, b0, &sc.state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;

    step("1. Bob precomputes T1 + 5 T2 per escrow, builds the covenants, funds E1, E2 in one ark tx");
    let t = Instant::now();
    let out2 = |u: u64| vec![(2u32, u)];
    let specs = vec![(600u64, vec![out2(600); 5]), (400u64, vec![out2(400); 5])];
    let pre = precompute(&bob, &info, &bob_rgb, cid, b0, &bob_addr, &specs, &sc.state, "escrow").await?;
    let timeout = unix_now() + timeout_s;
    let (bk, sk, ak) = (bkp.x_only_public_key().0.serialize(), sally_kp.x_only_public_key().0.serialize(), ari.x_only_public_key().0.serialize());
    let (sally_key, bob_key) = (out_key(&sally_addr), out_key(&bob_addr));
    let mut covs = vec![];
    for c in &pre.contracts {
        let l = |j: usize, rgb_key: [u8; 32], auth_key: [u8; 32], not_before: u64| Leaf { c1: c.t1.c, c2: c.t2[j].c, rgb_key, auth_key, not_before, ..Default::default() };
        let (xb, xs, xa) = (bkp.x_only_public_key().0, sally_kp.x_only_public_key().0, ari.x_only_public_key().0);
        covs.push(CovVtxo::new_full(&info, &emu, &[vec![xb, xs], vec![xb, xa], vec![xs, xa]], None, vec![
            l(0, sally_key, bk, 0), l(1, sally_key, ak, 0), l(2, bob_key, sk, 0), l(3, bob_key, ak, 0), l(4, bob_key, [0; 32], timeout),
        ], None)?);
    }
    sc.rec("precompute_ms", t.elapsed().as_millis().to_string());
    sc.rec("exit_leaves", exit_leaf_report(&covs[0]));
    ensure!(covs[0].exits.iter().all(|e| e.instructions().filter(|i| matches!(i, Ok(bitcoin::script::Instruction::PushBytes(b)) if b.len() == 32)).count() >= 2), "a single-key exit leaf is left");
    let (fund_tx, cp_a) = fund_contracts(&bob, bkp.x_only_public_key().0, &info, &pre, &[&covs[0], &covs[1]], &bob_addr).await?;
    let e1 = OutPoint { txid: fund_tx.compute_txid(), vout: 1 };
    let e2 = OutPoint { txid: fund_tx.compute_txid(), vout: 2 };
    sc.rec("escrows", format!("E1 {e1} (600), E2 {e2} (400); 5 outcomes each, proof {} B; timeout {timeout} (now+{timeout_s}s)", covs[0].proofs[0].len() / 2));
    let mut ctx = Ctx { state: sc.state.clone(), map: vec![(pre.t2f.ph_txid, fund_tx.clone())], offchain: vec![cp_a.compute_txid().to_string(), fund_tx.compute_txid().to_string()] };

    step("2. negatives on E1 (each refused by the emulator)");
    let c0 = &covs[0];
    let mk = |idx: usize, outs: Vec<TxOut>, sig: Vec<u8>| { let info = info.clone(); async move { cov_spend(&info, c0, e1, V_SAT, idx, None, outs, None, &sally_addr, &[], &sig).await } };
    let negs: Vec<(&str, CovSpend)> = vec![
        ("release_no_signature", mk(1, plain_outs(&sally_addr, &sally_addr), vec![]).await?),
        ("release_signed_by_seller", mk(0, plain_outs(&sally_addr, &sally_addr), auth_sig(&sally_kp, &c0.leaves[0], e1)).await?),
        ("release_arbiter_sig_for_E2", mk(1, plain_outs(&sally_addr, &sally_addr), auth_sig(&ari, &c0.leaves[1], e2)).await?),
        ("release_redirected_to_seller_other_key", mk(1, plain_outs(&sally_addr, &bob_addr), auth_sig(&ari, &c0.leaves[1], e1)).await?),
        ("refund_signed_by_buyer", mk(2, plain_outs(&bob_addr, &bob_addr), auth_sig(&bkp, &c0.leaves[2], e1)).await?),
        ("timeout_refund_early", mk(4, plain_outs(&bob_addr, &bob_addr), vec![]).await?),
    ];
    for (n, s) in negs {
        let why = ctx.refuse(&bob, s).await?;
        sc.rec(&format!("neg_{n}"), why);
    }
    sc.rec("e1_after_negatives", spent(e1).await);

    step("3. E1: the arbiter signs the release; Sally (or anyone) submits; Sally validates 600");
    let s = cov_spend(&info, c0, e1, V_SAT, 1, None, plain_outs(&sally_addr, &sally_addr), None, &sally_addr, &[], &auth_sig(&ari, &c0.leaves[1], e1)).await?;
    let (fin, txid, ms) = ctx.settle(&sally, s, &pre.contracts[0].t1, &pre.contracts[0].t2[1], "e1-release").await?;
    wait_vtxo(&sally, OutPoint { txid, vout: 2 }, "sally").await?;
    let u = valid_units(&sally_rgb, &sally_online, &fin, &ctx.offchain, txid, 2)?;
    sc.rec("e1_release_by_arbiter", format!("{u} units to Sally, ark tx {txid}, emulator {ms} ms, consignment {} B", std::fs::metadata(&fin)?.len()));
    ensure!(u == 600, "release gives {u}");

    step("4. E2: wait for the timeout (emulator clock), Bob takes the refund without signatures");
    let wait = timeout.saturating_sub(unix_now()) + 3;
    sc.rec("waited_s", wait.to_string());
    tokio::time::sleep(Duration::from_secs(wait)).await;
    let c1 = &covs[1];
    let s = cov_spend(&info, c1, e2, V_SAT, 4, None, plain_outs(&bob_addr, &bob_addr), None, &bob_addr, &[], &[]).await?;
    let (fin, txid, ms) = ctx.settle(&bob, s, &pre.contracts[1].t1, &pre.contracts[1].t2[4], "e2-timeout").await?;
    wait_vtxo(&bob, OutPoint { txid, vout: 2 }, "bob").await?;
    let u = valid_units(&bob_rgb, &bob_online, &fin, &ctx.offchain, txid, 2)?;
    sc.rec("e2_timeout_refund", format!("{u} units back to Bob, ark tx {txid}, emulator {ms} ms"));
    ensure!(u == 400, "refund gives {u}");
    println!("EMU-ESCROW RESULT: PASS");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Scenario 5: emu-lending. Bob (borrower) locks 1000 units of collateral in three loans L1..L3
// (one funding tx). Lena (lender) lent BTC out of band. Oracle key O signs price statements.
// Outcomes per loan:
//   0 repay:     output 1 pays >= REPAY to Lena, collateral -> Bob (pinned), before maturity M
//   1 liquidate: oracle statement price < THRESHOLD (fresh), collateral -> Lena (pinned)
//   2 default:   after M, collateral -> Lena (pinned)
// ---------------------------------------------------------------------------------------------
const REPAY_SAT: u64 = 25_000;
const THRESHOLD: u64 = 50_000;

pub async fn run_lending() -> anyhow::Result<()> {
    let mut sc = Scn::new("emu-lending")?;
    let regtest = Arc::new(Regtest::new());
    let maturity_s: u64 = std::env::var("EMU_LOAN_MATURITY_S").ok().and_then(|s| s.parse().ok()).unwrap_or(150);
    step("0. emulator, clients (Bob borrower with the asset, Lena lender; oracle key)");
    emulator_up("emulator:rgb", &[])?;
    let emu = emu_pubkey().await?;
    let (bob, bkp) = keypair_client("bob", regtest.clone(), &sc.state).await?;
    let (lena, _) = ark_client("lena", regtest.clone(), &sc.state).await?;
    let info = bob.server_info().await?;
    board("bob", &bob, "0.001").await?;
    let b0 = first(&bob, "bob").await?;
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (lena_addr, _) = lena.get_offchain_address().await?;
    let oracle = new_keypair();
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &sc.state))?;
    let (lena_rgb, lena_online) = bl(|| rgb_wallet("lena", &sc.state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut bob_rgb, &bob_online, b0, &sc.state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;

    step("1. Bob precomputes (T1 + 3 T2 per loan), builds the covenants, funds L1..L3 in one ark tx");
    let t = Instant::now();
    let out2 = |u: u64| vec![(2u32, u)];
    let specs = vec![(400u64, vec![out2(400); 3]), (300u64, vec![out2(300); 3]), (300u64, vec![out2(300); 3])];
    let pre = precompute(&bob, &info, &bob_rgb, cid, b0, &bob_addr, &specs, &sc.state, "loan").await?;
    let maturity = unix_now() + maturity_s;
    let (bob_key, lena_key) = (out_key(&bob_addr), out_key(&lena_addr));
    let ok = oracle.x_only_public_key().0.serialize();
    let lena_kp = new_keypair();
    let exits = [vec![bkp.x_only_public_key().0, lena_kp.x_only_public_key().0]];
    let mut covs = vec![];
    for c in &pre.contracts {
        covs.push(CovVtxo::new_full(&info, &emu, &exits, None, vec![
            Leaf { c1: c.t1.c, c2: c.t2[0].c, pay_key: lena_key, pay_min: REPAY_SAT, rgb_key: bob_key, not_after: maturity, ..Default::default() },
            Leaf { c1: c.t1.c, c2: c.t2[1].c, rgb_key: lena_key, auth_key: ok, price_below: THRESHOLD, ..Default::default() },
            Leaf { c1: c.t1.c, c2: c.t2[2].c, rgb_key: lena_key, not_before: maturity, ..Default::default() },
        ], None)?);
    }
    sc.rec("precompute_ms", t.elapsed().as_millis().to_string());
    sc.rec("exit_leaves", exit_leaf_report(&covs[0]));
    let (fund_tx, cp_a) = fund_contracts(&bob, bkp.x_only_public_key().0, &info, &pre, &[&covs[0], &covs[1], &covs[2]], &bob_addr).await?;
    let ft = fund_tx.compute_txid();
    let l: Vec<OutPoint> = (1..=3).map(|v| OutPoint { txid: ft, vout: v }).collect();
    let bob_btc = OutPoint { txid: ft, vout: 4 };
    sc.rec("loans", format!("L1 {} (400), L2 {} (300), L3 {} (300); maturity {maturity} (now+{maturity_s}s); Bob BTC {bob_btc}", l[0], l[1], l[2]));
    wait_vtxo(&bob, bob_btc, "bob").await?;
    let mut ctx = Ctx { state: sc.state.clone(), map: vec![(pre.t2f.ph_txid, fund_tx.clone())], offchain: vec![cp_a.compute_txid().to_string(), ft.to_string()] };
    let repay_outs = |pay: u64, rgb_to: &ArkAddress| vec![
        TxOut { value: Amount::from_sat(pay), script_pubkey: lena_addr.to_p2tr_script_pubkey() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: rgb_to.to_p2tr_script_pubkey() },
    ];

    step("2. negatives (each refused by the emulator)");
    let now0 = unix_now();
    let (hi_stmt, hi_sig) = oracle_stmt(&oracle, 60_000, now0);
    let forged = oracle_stmt(&new_keypair(), 10_000, now0);
    let (stale_stmt, stale_sig) = oracle_stmt(&oracle, 10_000, now0 - 3_700);
    let negs: Vec<(&str, CovSpend)> = vec![
        ("repay_underpaid", cov_spend(&info, &covs[0], l[0], V_SAT, 0, None, repay_outs(REPAY_SAT - 1, &bob_addr), Some((&bob, bob_btc)), &bob_addr, &[], &[]).await?),
        ("repay_collateral_to_lender", cov_spend(&info, &covs[0], l[0], V_SAT, 0, None, repay_outs(REPAY_SAT, &lena_addr), Some((&bob, bob_btc)), &bob_addr, &[], &[]).await?),
        ("liquidate_price_above_threshold", cov_spend(&info, &covs[1], l[1], V_SAT, 1, None, plain_outs(&lena_addr, &lena_addr), None, &lena_addr, &hi_stmt, &hi_sig).await?),
        ("liquidate_forged_oracle", cov_spend(&info, &covs[1], l[1], V_SAT, 1, None, plain_outs(&lena_addr, &lena_addr), None, &lena_addr, &forged.0, &forged.1).await?),
        ("liquidate_stale_statement", cov_spend(&info, &covs[1], l[1], V_SAT, 1, None, plain_outs(&lena_addr, &lena_addr), None, &lena_addr, &stale_stmt, &stale_sig).await?),
        ("default_before_maturity", cov_spend(&info, &covs[2], l[2], V_SAT, 2, None, plain_outs(&lena_addr, &lena_addr), None, &lena_addr, &[], &[]).await?),
    ];
    for (n, s) in negs {
        let why = ctx.refuse(&lena, s).await?;
        sc.rec(&format!("neg_{n}"), why);
    }

    step("3. L1: Bob repays (pays Lena in the same ark tx) and gets the collateral back");
    let s = cov_spend(&info, &covs[0], l[0], V_SAT, 0, None, repay_outs(REPAY_SAT, &bob_addr), Some((&bob, bob_btc)), &bob_addr, &[], &[]).await?;
    let (fin, txid, ms) = ctx.settle(&bob, s, &pre.contracts[0].t1, &pre.contracts[0].t2[0], "l1-repay").await?;
    wait_vtxo(&lena, OutPoint { txid, vout: 1 }, "lena").await?;
    let u = valid_units(&bob_rgb, &bob_online, &fin, &ctx.offchain, txid, 2)?;
    sc.rec("l1_repaid", format!("{u} units back to Bob, Lena paid {REPAY_SAT} sat at {txid}:1, emulator {ms} ms"));
    ensure!(u == 400, "repay gives {u}");
    let bob_btc2 = OutPoint { txid, vout: 3 };

    step("4. L2: the oracle publishes price 42000 < 50000; Lena liquidates");
    let (st, sg) = oracle_stmt(&oracle, 42_000, unix_now());
    let s = cov_spend(&info, &covs[1], l[1], V_SAT, 1, None, plain_outs(&lena_addr, &lena_addr), None, &lena_addr, &st, &sg).await?;
    let (fin, txid, ms) = ctx.settle(&lena, s, &pre.contracts[1].t1, &pre.contracts[1].t2[1], "l2-liquidate").await?;
    wait_vtxo(&lena, OutPoint { txid, vout: 2 }, "lena").await?;
    let u = valid_units(&lena_rgb, &lena_online, &fin, &ctx.offchain, txid, 2)?;
    sc.rec("l2_liquidated", format!("{u} units to Lena, ark tx {txid}, emulator {ms} ms"));
    ensure!(u == 300, "liquidation gives {u}");

    step("5. L3: after maturity a repay is refused, the default outcome gives the collateral to Lena");
    let wait = maturity.saturating_sub(unix_now()) + 3;
    sc.rec("waited_s", wait.to_string());
    tokio::time::sleep(Duration::from_secs(wait)).await;
    let late = cov_spend(&info, &covs[2], l[2], V_SAT, 0, None, repay_outs(REPAY_SAT, &bob_addr), Some((&bob, bob_btc2)), &bob_addr, &[], &[]).await?;
    sc.rec("neg_repay_after_maturity", ctx.refuse(&bob, late).await?);
    let s = cov_spend(&info, &covs[2], l[2], V_SAT, 2, None, plain_outs(&lena_addr, &lena_addr), None, &lena_addr, &[], &[]).await?;
    let (fin, txid, ms) = ctx.settle(&lena, s, &pre.contracts[2].t1, &pre.contracts[2].t2[2], "l3-default").await?;
    wait_vtxo(&lena, OutPoint { txid, vout: 2 }, "lena").await?;
    let u = valid_units(&lena_rgb, &lena_online, &fin, &ctx.offchain, txid, 2)?;
    sc.rec("l3_default", format!("{u} units to Lena, ark tx {txid}, emulator {ms} ms"));
    ensure!(u == 300, "default gives {u}");
    println!("EMU-LENDING RESULT: PASS");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Scenario 4: emu-vault. Olivia's asset sits in vault VTXOs X1, X2 (one funding tx). The hot
// key H may unvault only into a precommitted pending VTXO U_i, and only inside time slot i; from
// U_i the asset can go only to the precommitted destination D, and only DELAY seconds after the
// END of slot i: a relative delay quantized by the slot length, built from absolute emulator-clock
// windows. The cold key C claws back from X or from any U_i at any time, to the cold address.
// ---------------------------------------------------------------------------------------------
struct VaultPre {
    t1: Pre,
    unvault: Vec<Pre>, // per slot: CP:1 -> vout 3 (U_i)
    claw: Pre,
    u: Vec<(Pre, Pre, Pre)>, // per slot: U_i's T1, withdraw (-> vout 2 = D), clawback (-> vout 2 = cold)
}

pub async fn run_vault() -> anyhow::Result<()> {
    let mut sc = Scn::new("emu-vault")?;
    let regtest = Arc::new(Regtest::new());
    let slot_s: u64 = std::env::var("EMU_VAULT_SLOT_S").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let delay_s: u64 = std::env::var("EMU_VAULT_DELAY_S").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let n_slots = 4usize;
    step("0. emulator, clients (Olivia owner; hot key H, cold key C; cold and destination wallets)");
    emulator_up("emulator:rgb", &[])?;
    let emu = emu_pubkey().await?;
    let (olivia, okp) = keypair_client("olivia", regtest.clone(), &sc.state).await?;
    let (cold, _) = ark_client("cold", regtest.clone(), &sc.state).await?;
    let (dest, _) = ark_client("dest", regtest.clone(), &sc.state).await?;
    let info = olivia.server_info().await?;
    board("olivia", &olivia, "0.001").await?;
    let o0 = first(&olivia, "olivia").await?;
    let (o_addr, _) = olivia.get_offchain_address().await?;
    let (cold_addr, _) = cold.get_offchain_address().await?;
    let (dest_addr, _) = dest.get_offchain_address().await?;
    let (hot, ckey) = (new_keypair(), new_keypair());
    let (mut o_rgb, o_online) = bl(|| rgb_wallet("olivia", &sc.state))?;
    let (cold_rgb, cold_online) = bl(|| rgb_wallet("cold", &sc.state))?;
    let (dest_rgb, dest_online) = bl(|| rgb_wallet("dest", &sc.state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut o_rgb, &o_online, o0, &sc.state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;

    step("1. Olivia precomputes (per vault: T1, 4 unvault slots, clawback; per U_i: T1, withdraw, clawback) and funds X1, X2");
    let t = Instant::now();
    let a_in = olivia.resolve_selected_send_inputs(&[o0]).await?.remove(0);
    let dry = build_offchain_transactions(&[SendReceiver::bitcoin(o_addr.clone(), Amount::from_sat(V_SAT)), SendReceiver::bitcoin(o_addr.clone(), Amount::from_sat(V_SAT))], &o_addr, &[a_in.clone().with_checkpoint_op_return(Some([0u8; 32]))], &info)?;
    let mut cp_a = dry.checkpoint_txs[0].clone();
    cp_a.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    let units = [600u64, 400u64];
    let t1a = precolor(&o_rgb, &mut cp_a, cid, vec![(SealTarget::WitnessVout(1), ISSUE_AMOUNT, 9900)], sc.state.join("vault-t1a.rgbc"))?;
    let mut fp = placeholder(&[OutPoint { txid: t1a.ph_txid, vout: 1 }], 3, 0xf2);
    let t2f = precolor(&o_rgb, &mut fp, cid, vec![(SealTarget::WitnessVout(1), units[0], 9901), (SealTarget::WitnessVout(2), units[1], 9902)], sc.state.join("vault-t2f.rgbc"))?;
    let mut vp: Vec<VaultPre> = vec![];
    for (x, u) in units.iter().enumerate() {
        let f = |n: &str| sc.state.join(format!("vault-x{x}-{n}.rgbc"));
        let bl0 = 10_000 + 1_000 * x as u64;
        let mut cp = placeholder(&[OutPoint { txid: t2f.ph_txid, vout: 1 + x as u32 }], 3, 0x30 + x as u8);
        let t1 = precolor(&o_rgb, &mut cp, cid, vec![(SealTarget::WitnessVout(1), *u, bl0)], f("t1"))?;
        let cp1 = OutPoint { txid: t1.ph_txid, vout: 1 };
        let mut p = placeholder(&[OutPoint::null(), cp1], 3, 0x40 + x as u8);
        let claw = precolor(&o_rgb, &mut p, cid, vec![(SealTarget::WitnessVout(2), *u, bl0 + 1)], f("claw"))?;
        let (mut unvault, mut us) = (vec![], vec![]);
        for i in 0..n_slots {
            let b = bl0 + 10 * (i as u64 + 1);
            let mut p = placeholder(&[OutPoint::null(), cp1], 4, 0x50 + (x * n_slots + i) as u8);
            let un = precolor(&o_rgb, &mut p, cid, vec![(SealTarget::WitnessVout(3), *u, b)], f(&format!("unvault{i}")))?;
            let mut cpu = placeholder(&[OutPoint { txid: un.ph_txid, vout: 3 }], 3, 0x70 + (x * n_slots + i) as u8);
            let ut1 = precolor(&o_rgb, &mut cpu, cid, vec![(SealTarget::WitnessVout(1), *u, b + 1)], f(&format!("u{i}-t1")))?;
            let ucp1 = OutPoint { txid: ut1.ph_txid, vout: 1 };
            let mut pw = placeholder(&[OutPoint::null(), ucp1], 3, 0x90 + (x * n_slots + i) as u8);
            let w = precolor(&o_rgb, &mut pw, cid, vec![(SealTarget::WitnessVout(2), *u, b + 2)], f(&format!("u{i}-withdraw")))?;
            let mut pc = placeholder(&[OutPoint::null(), ucp1], 3, 0xb0 + (x * n_slots + i) as u8);
            let c = precolor(&o_rgb, &mut pc, cid, vec![(SealTarget::WitnessVout(2), *u, b + 3)], f(&format!("u{i}-claw")))?;
            unvault.push(un);
            us.push((ut1, w, c));
        }
        vp.push(VaultPre { t1, unvault, claw, u: us });
    }
    let s0 = unix_now();
    let slot = |i: usize| s0 + slot_s * i as u64;
    let (hk, ck) = (hot.x_only_public_key().0.serialize(), ckey.x_only_public_key().0.serialize());
    let (cold_key, dest_key) = (out_key(&cold_addr), out_key(&dest_addr));
    let owner = okp.x_only_public_key().0;
    let mut xs: Vec<CovVtxo> = vec![];
    let mut uss: Vec<Vec<CovVtxo>> = vec![];
    for v in &vp {
        let mut ucovs = vec![];
        for (i, (ut1, w, c)) in v.u.iter().enumerate() {
            ucovs.push(CovVtxo::new(&info, &emu, owner, vec![
                Leaf { c1: ut1.c, c2: w.c, rgb_key: dest_key, not_before: slot(i + 1) + delay_s, ..Default::default() },
                Leaf { c1: ut1.c, c2: c.c, rgb_key: cold_key, auth_key: ck, ..Default::default() },
            ])?);
        }
        let mut leaves: Vec<Leaf> = (0..n_slots).map(|i| Leaf { c1: v.t1.c, c2: v.unvault[i].c, next_key: ucovs[i].key(), next_min: V_SAT, auth_key: hk, not_before: slot(i), not_after: slot(i + 1), ..Default::default() }).collect();
        leaves.push(Leaf { c1: v.t1.c, c2: v.claw.c, rgb_key: cold_key, auth_key: ck, ..Default::default() });
        xs.push(CovVtxo::new(&info, &emu, owner, leaves)?);
        uss.push(ucovs);
    }
    sc.rec("precompute_ms", format!("{} ({} transitions)", t.elapsed().as_millis(), 2 + units.len() * (2 + 4 * n_slots)));
    sc.rec("slots", format!("{n_slots} x {slot_s}s from {s0}; delay {delay_s}s after the slot end"));
    // funding: [OP_RETURN T2f, X1, X2, change, anchor]
    let built = build_offchain_transactions(&[SendReceiver::bitcoin(xs[0].addr.clone(), Amount::from_sat(V_SAT)), SendReceiver::bitcoin(xs[1].addr.clone(), Amount::from_sat(V_SAT))], &o_addr, &[a_in.with_checkpoint_op_return(Some(t1a.c))], &info)?;
    let mut ark = built.ark_tx.clone();
    ark.unsigned_tx.output[2].value -= Amount::from_sat(OPRET_SAT);
    ark.unsigned_tx.output.insert(0, TxOut { value: Amount::from_sat(OPRET_SAT), script_pubkey: ScriptBuf::new_op_return(t2f.c) });
    ark.outputs.insert(0, psbt::Output::default());
    let fund_tx = ark.unsigned_tx.clone();
    let cp_a_real = built.checkpoint_txs[0].unsigned_tx.clone();
    ensure!(cp_a_real.compute_txid() == t1a.ph_txid, "checkpoint differs");
    let pend = olivia.submit_built_offchain_send(ark, built.checkpoint_txs, owner).await.map_err(|e| anyhow!("fund: {e:?}"))?;
    olivia.sign_and_finalize_pending_tx(pend).await.map_err(|e| anyhow!("fund finalize: {e:?}"))?;
    let ft = fund_tx.compute_txid();
    let x = [OutPoint { txid: ft, vout: 1 }, OutPoint { txid: ft, vout: 2 }];
    sc.rec("vaults", format!("X1 {} (600), X2 {} (400); X proof {} B, U proof {} B", x[0], x[1], xs[0].proofs[0].len() / 2, uss[0][0].proofs[0].len() / 2));
    let mut ctx = Ctx { state: sc.state.clone(), map: vec![(t2f.ph_txid, fund_tx.clone())], offchain: vec![cp_a_real.compute_txid().to_string(), ft.to_string()] };
    let cur_slot = || ((unix_now() - s0) / slot_s) as usize;
    step("2. X1: negatives, then the hot key unvaults in the current slot; a withdraw before the delay is refused; the cold key claws back from U");
    let i = cur_slot();
    ensure!(i + 1 < n_slots, "too late for slot tests ({i})");
    // vout layout needs out1/out2 before out3: the payer (Olivia) funds them
    let o_btc = OutPoint { txid: ft, vout: 3 };
    wait_vtxo(&olivia, o_btc, "olivia").await?;
    let pre_outs = |u: &CovVtxo| {
        let mut m = vec![
            TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: o_addr.to_p2tr_script_pubkey() },
            TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: o_addr.to_p2tr_script_pubkey() },
        ];
        m.push(TxOut { value: Amount::from_sat(V_SAT), script_pubkey: u.addr.to_p2tr_script_pubkey() });
        m
    };
    let x1 = &xs[0];
    let negs: Vec<(&str, CovSpend)> = vec![
        ("unvault_signed_by_cold_key", cov_spend(&info, x1, x[0], V_SAT, i, None, pre_outs(&uss[0][i]), Some((&olivia, o_btc)), &o_addr, &[], &auth_sig(&ckey, &x1.leaves[i], x[0])).await?),
        ("unvault_next_slot_leaf", cov_spend(&info, x1, x[0], V_SAT, i + 1, None, pre_outs(&uss[0][i + 1]), Some((&olivia, o_btc)), &o_addr, &[], &auth_sig(&hot, &x1.leaves[i + 1], x[0])).await?),
        ("unvault_to_other_pending_vtxo", cov_spend(&info, x1, x[0], V_SAT, i, None, pre_outs(&uss[0][(i + 1) % n_slots]), Some((&olivia, o_btc)), &o_addr, &[], &auth_sig(&hot, &x1.leaves[i], x[0])).await?),
        ("clawback_signed_by_hot_key", cov_spend(&info, x1, x[0], V_SAT, n_slots, None, plain_outs(&cold_addr, &cold_addr), None, &cold_addr, &[], &auth_sig(&hot, &x1.leaves[n_slots], x[0])).await?),
    ];
    for (n, s) in negs {
        sc.rec(&format!("neg_{n}"), ctx.refuse(&olivia, s).await?);
    }
    let s = cov_spend(&info, x1, x[0], V_SAT, i, None, pre_outs(&uss[0][i]), Some((&olivia, o_btc)), &o_addr, &[], &auth_sig(&hot, &x1.leaves[i], x[0])).await?;
    let (fin, txid, ms) = ctx.settle(&olivia, s, &vp[0].t1, &vp[0].unvault[i], "x1-unvault").await?;
    let u1 = OutPoint { txid, vout: 3 };
    let o_btc2 = OutPoint { txid, vout: 4 };
    wait_vtxo(&olivia, o_btc2, "olivia").await?;
    let n_u = valid_units(&o_rgb, &o_online, &fin, &ctx.offchain, txid, 3)?;
    sc.rec("x1_unvaulted", format!("slot {i}: {n_u} units on U {u1}, emulator {ms} ms"));
    ensure!(n_u == 600, "unvault moves {n_u}");
    let u_cov = &uss[0][i];
    let early = cov_spend(&info, u_cov, u1, V_SAT, 0, None, plain_outs(&dest_addr, &dest_addr), None, &dest_addr, &[], &[]).await?;
    sc.rec("neg_withdraw_before_delay", ctx.refuse(&olivia, early).await?);
    let s = cov_spend(&info, u_cov, u1, V_SAT, 1, None, plain_outs(&cold_addr, &cold_addr), None, &cold_addr, &[], &auth_sig(&ckey, &u_cov.leaves[1], u1)).await?;
    let (fin, txid, ms) = ctx.settle(&olivia, s, &vp[0].u[i].0, &vp[0].u[i].2, "x1-clawback").await?;
    wait_vtxo(&cold, OutPoint { txid, vout: 2 }, "cold").await?;
    let n_c = valid_units(&cold_rgb, &cold_online, &fin, &ctx.offchain, txid, 2)?;
    sc.rec("x1_clawed_back_from_u", format!("{n_c} units to the cold address, emulator {ms} ms"));
    ensure!(n_c == 600, "clawback gives {n_c}");

    step("3. X2: hot key unvaults in the current slot, waits slot end + delay, withdraws to D");
    let j = cur_slot();
    ensure!(j < n_slots, "slots over ({j})");
    let x2 = &xs[1];
    let s = cov_spend(&info, x2, x[1], V_SAT, j, None, pre_outs(&uss[1][j]), Some((&olivia, o_btc2)), &o_addr, &[], &auth_sig(&hot, &x2.leaves[j], x[1])).await?;
    let (_, txid, _) = ctx.settle(&olivia, s, &vp[1].t1, &vp[1].unvault[j], "x2-unvault").await?;
    let u2 = OutPoint { txid, vout: 3 };
    wait_vtxo(&olivia, OutPoint { txid, vout: 4 }, "olivia").await?;
    let ready = slot(j + 1) + delay_s;
    let wait = ready.saturating_sub(unix_now()) + 3;
    sc.rec("x2_unvaulted", format!("slot {j}, U {u2}; withdraw allowed at {ready}, waiting {wait}s"));
    tokio::time::sleep(Duration::from_secs(wait)).await;
    let u_cov = &uss[1][j];
    let s = cov_spend(&info, u_cov, u2, V_SAT, 0, None, plain_outs(&dest_addr, &dest_addr), None, &dest_addr, &[], &[]).await?;
    let (fin, txid, ms) = ctx.settle(&olivia, s, &vp[1].u[j].0, &vp[1].u[j].1, "x2-withdraw").await?;
    wait_vtxo(&dest, OutPoint { txid, vout: 2 }, "dest").await?;
    let n_d = valid_units(&dest_rgb, &dest_online, &fin, &ctx.offchain, txid, 2)?;
    sc.rec("x2_withdrawn", format!("{n_d} units to D, emulator {ms} ms, consignment {} B", std::fs::metadata(&fin)?.len()));
    ensure!(n_d == 400, "withdraw gives {n_d}");
    println!("EMU-VAULT RESULT: PASS");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Scenario 6: emu-asset-swap. Alice sells 1000 RGB units for PRICE_X units of an Arkade asset X
// (issued by Carol on Ark). The covenant checks the Arkade-asset leg natively: output 1 is
// Alice's key AND carries >= PRICE_X of X according to the tx's asset packet
// (OP_INSPECTOUTASSETLOOKUP), which arkd validates against the inputs. RGB leg as in emu-lots.
// ---------------------------------------------------------------------------------------------
const PRICE_X: u64 = 1_500;
const X_ISSUED: u64 = 5_000;

pub async fn run_asset_swap() -> anyhow::Result<()> {
    let mut sc = Scn::new("emu-asset-swap")?;
    let regtest = Arc::new(Regtest::new());
    step("0. emulator, clients; Carol issues the Arkade asset X and consolidates it with her BTC");
    emulator_up("emulator:rgb", &[])?;
    let emu = emu_pubkey().await?;
    let (alice, akp) = keypair_client("alice", regtest.clone(), &sc.state).await?;
    let (carol, _) = ark_client("carol", regtest.clone(), &sc.state).await?;
    let info = alice.server_info().await?;
    board("alice", &alice, "0.001").await?;
    board("carol", &carol, "0.001").await?;
    let a0 = first(&alice, "alice").await?;
    let (alice_addr, _) = alice.get_offchain_address().await?;
    let (carol_addr, _) = carol.get_offchain_address().await?;
    let iss = carol.issue_asset(X_ISSUED, None, None).await.map_err(|e| anyhow!("issue_asset: {e:?}"))?;
    let x = *iss.asset_ids.last().ok_or_else(|| anyhow!("no asset id"))?;
    sc.rec("arkade_asset", format!("{x:?} issued {X_ISSUED} in ark tx {}", iss.ark_txid));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mine: Vec<OutPoint> = carol.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().map(|v| v.vtxo().outpoint).collect();
    let ins = carol.resolve_selected_send_inputs(&mine).await?;
    let total: u64 = ins.iter().map(|i| i.amount().to_sat()).sum();
    let recv = vec![SendReceiver { address: carol_addr.clone(), amount: Amount::from_sat(total), assets: vec![ark_core::server::Asset { asset_id: x, amount: X_ISSUED }] }];
    let tx = ark_core::send::build_asset_send_transactions(&recv, &carol_addr, &ins, &info)?;
    let ctxid = tx.ark_tx.unsigned_tx.compute_txid();
    let (_, cvtxo) = carol.get_offchain_address().await?;
    let p = carol.submit_built_offchain_send(tx.ark_tx, tx.checkpoint_txs, cvtxo.owner_pk()).await.map_err(|e| anyhow!("consolidate: {e:?}"))?;
    carol.sign_and_finalize_pending_tx(p).await.map_err(|e| anyhow!("consolidate finalize: {e:?}"))?;
    let c0 = OutPoint { txid: ctxid, vout: 0 };
    wait_vtxo(&carol, c0, "carol").await?;
    let ci = crate::long_offline::vtxo_info(c0).await.ok_or_else(|| anyhow!("no info for {c0}"))?;
    sc.rec("carol_vtxo", format!("{c0}: {} sat, assets {:?}", ci.amount.to_sat(), ci.assets));
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &sc.state))?;
    let (carol_rgb, carol_online) = bl(|| rgb_wallet("carol", &sc.state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, a0, &sc.state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;

    step("1. Alice precomputes T1/T2, builds the asset-checking covenant, funds the offer");
    let pre = precompute(&alice, &info, &alice_rgb, cid, a0, &alice_addr, &[(ISSUE_AMOUNT, vec![vec![(2u32, ISSUE_AMOUNT)]])], &sc.state, "ax").await?;
    let c = &pre.contracts[0];
    let leaf = Leaf { c1: c.t1.c, c2: c.t2[0].c, pay_key: out_key(&alice_addr), pay_min: CARRIER_SAT, ..Default::default() };
    let cov = CovVtxo::new_with(&info, &emu, akp.x_only_public_key().0, vec![leaf], Some((x, PRICE_X)))?;
    sc.rec("covenant_bytes", (cov.covenant.len() / 2).to_string());
    let (fund_tx, cp_a) = fund_contracts(&alice, akp.x_only_public_key().0, &info, &pre, &[&cov], &alice_addr).await?;
    let v = OutPoint { txid: fund_tx.compute_txid(), vout: 1 };
    sc.rec("offer", format!("{v}: {ISSUE_AMOUNT} RGB units for {PRICE_X} X"));
    let mut ctx = Ctx { state: sc.state.clone(), map: vec![(pre.t2f.ph_txid, fund_tx.clone())], offchain: vec![cp_a.compute_txid().to_string(), fund_tx.compute_txid().to_string()] };
    let outs_ = || vec![
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: alice_addr.to_p2tr_script_pubkey() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: carol_addr.to_p2tr_script_pubkey() },
    ];

    step("2. negatives: X underpaid, X to the change instead of Alice, no asset packet");
    let negs: Vec<(&str, CovSpend)> = vec![
        ("asset_underpaid", cov_spend_x(&info, &cov, v, V_SAT, 0, None, outs_(), Some((&carol, c0)), &carol_addr, &[], &[], Some((x, X_ISSUED, PRICE_X - 1))).await?),
        ("asset_not_on_output1", cov_spend_x(&info, &cov, v, V_SAT, 0, None, outs_(), Some((&carol, c0)), &carol_addr, &[], &[], Some((x, X_ISSUED, 0))).await?),
        ("btc_only_no_asset_packet", cov_spend(&info, &cov, v, V_SAT, 0, None, outs_(), Some((&carol, c0)), &carol_addr, &[], &[]).await?),
    ];
    for (n, s) in negs {
        let why = ctx.refuse(&carol, s).await;
        sc.rec(&format!("neg_{n}"), format!("{why:?}"));
        why?;
    }

    step("3. Carol pays 1500 X + gets 1000 RGB units in one ark tx");
    let s = cov_spend_x(&info, &cov, v, V_SAT, 0, None, outs_(), Some((&carol, c0)), &carol_addr, &[], &[], Some((x, X_ISSUED, PRICE_X))).await?;
    sc.rec("fill_tx", format!("{} vB, extension {} B", s.psbt.unsigned_tx.vsize(), s.ext_bytes));
    let (fin, txid, ms) = ctx.settle(&carol, s, &c.t1, &c.t2[0], "asset-swap").await?;
    wait_vtxo(&carol, OutPoint { txid, vout: 2 }, "carol").await?;
    let u = valid_units(&carol_rgb, &carol_online, &fin, &ctx.offchain, txid, 2)?;
    let paid = crate::long_offline::vtxo_info(OutPoint { txid, vout: 1 }).await.map(|i| format!("{} sat, assets {:?}", i.amount.to_sat(), i.assets)).unwrap_or_default();
    let chg = crate::long_offline::vtxo_info(OutPoint { txid, vout: 3 }).await.map(|i| format!("{} sat, assets {:?}", i.amount.to_sat(), i.assets)).unwrap_or_default();
    sc.rec("swap", format!("Carol {u} RGB units at {txid}:2; Alice {txid}:1 = {paid}; Carol change {txid}:3 = {chg}; emulator {ms} ms"));
    ensure!(u == ISSUE_AMOUNT, "Carol got {u}");
    println!("EMU-ASSET-SWAP RESULT: PASS");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Scenario emu-exit: the unilateral exit path of a covenant VTXO, exercised on-chain.
// One funding tx creates three contracts:
//   X_old  (300) legacy exit leaf = CSV + Bob alone (what the offers use)
//   X_esc  (300) escrow exits = CSV 2-of-2 for each pair of {Bob, Sally, Ari}
//   X_loan (400) lending exit = CSV 2-of-2 {Bob, Lena}; Bob pre-signs, at funding time, a default
//                tx to Lena with nLockTime = maturity (arkd closures cannot carry CLTV in an exit)
// Bob unrolls the funding tx (operator and emulator not involved), waits the CSV, then:
//   legacy: Bob alone takes X_old with the precomputed T1          -> accepted (the bypass)
//   X_esc / X_loan: Bob alone                                     -> refused by consensus
//   X_esc: Bob + Ari (arbiter-backed refund)                       -> accepted
//   X_loan: Lena with Bob's pre-signed default, before / after the locktime -> refused / accepted
// ---------------------------------------------------------------------------------------------
const EXIT_FEE_SAT: u64 = 400;

/// On-chain spend of covenant VTXO `v` through exit leaf `leaf`: outputs [OP_RETURN c1, `to` <-
/// V_SAT - fee]. `signers[i]` signs for the i-th key of the leaf (None = missing signature).
fn exit_tx(cov: &CovVtxo, leaf: usize, v: OutPoint, c1: [u8; 32], to: ScriptBuf, lock: u32, signers: &[Option<&bitcoin::key::Keypair>]) -> anyhow::Result<Transaction> {
    use bitcoin::hashes::Hash;
    use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
    use bitcoin::taproot::{LeafVersion, TapLeafHash};
    let script = cov.exits[leaf].clone();
    let seq = cov.vtxo.exit_delay();
    let mut tx = Transaction {
        version: bitcoin::transaction::Version(2),
        lock_time: bitcoin::absolute::LockTime::from_consensus(lock),
        input: vec![TxIn { previous_output: v, sequence: seq, ..Default::default() }],
        output: vec![
            TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return(c1) },
            TxOut { value: Amount::from_sat(V_SAT - EXIT_FEE_SAT), script_pubkey: to },
        ],
    };
    let prev = TxOut { value: Amount::from_sat(V_SAT), script_pubkey: cov.addr.to_p2tr_script_pubkey() };
    let lh = TapLeafHash::from_script(&script, LeafVersion::TapScript);
    let sh = SighashCache::new(&tx).taproot_script_spend_signature_hash(0, &Prevouts::All(&[prev]), lh, TapSighashType::Default)?;
    let msg = bitcoin::secp256k1::Message::from_digest(sh.to_byte_array());
    let secp = Secp256k1::new();
    let mut w = bitcoin::Witness::new();
    for s in signers.iter().rev() {
        match s {
            Some(kp) => w.push(secp.sign_schnorr_no_aux_rand(&msg, kp).serialize()),
            None => w.push([]),
        }
    }
    w.push(script.as_bytes());
    w.push(cov.vtxo.get_spend_info(script.clone())?.serialize());
    tx.input[0].witness = w;
    Ok(tx)
}

/// testmempoolaccept: Ok(()) if accepted, Err(reject reason) otherwise.
fn mempool_check(regtest: &Regtest, tx: &Transaction) -> Result<(), String> {
    let r = regtest.rpc("testmempoolaccept", serde_json::json!([[bitcoin::consensus::encode::serialize_hex(tx)]])).map_err(|e| e.to_string())?;
    if r[0]["allowed"].as_bool() == Some(true) { Ok(()) } else { Err(r[0]["reject-reason"].as_str().unwrap_or("?").to_string() + " " + r[0]["reject-details"].as_str().unwrap_or("")) }
}

fn p2tr_of(kp: &bitcoin::key::Keypair) -> ScriptBuf {
    p2tr(kp.x_only_public_key().0.serialize())
}

pub async fn run_exit() -> anyhow::Result<()> {
    use crate::swap_ni::{branch_any, height, mine_to};
    let mut sc = Scn::new("emu-exit")?;
    let regtest = Arc::new(Regtest::new());
    step("0. emulator, Bob (buyer/borrower, has the asset); keys Sally, Ari (arbiter), Lena (lender)");
    emulator_up("emulator:rgb", &[])?;
    let emu = emu_pubkey().await?;
    let (bob, bkp) = keypair_client("bob", regtest.clone(), &sc.state).await?;
    let info = bob.server_info().await?;
    board("bob", &bob, "0.001").await?;
    let b0 = first(&bob, "bob").await?;
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (sally, ari, lena) = (new_keypair(), new_keypair(), new_keypair());
    let (xb, xs, xa, xl) = (bkp.x_only_public_key().0, sally.x_only_public_key().0, ari.x_only_public_key().0, lena.x_only_public_key().0);
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &sc.state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut bob_rgb, &bob_online, b0, &sc.state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;

    step("1. precompute + three covenant VTXOs with different exit leaf sets, one funding tx");
    let specs = vec![(300u64, vec![vec![(2u32, 300u64)]]), (300u64, vec![vec![(2u32, 300u64)]]), (400u64, vec![vec![(2u32, 400u64)]])];
    let pre = precompute(&bob, &info, &bob_rgb, cid, b0, &bob_addr, &specs, &sc.state, "exit").await?;
    let leaf = |i: usize| Leaf { c1: pre.contracts[i].t1.c, c2: pre.contracts[i].t2[0].c, rgb_key: out_key(&bob_addr), not_before: unix_now() + 3600, ..Default::default() };
    let x_old = CovVtxo::new(&info, &emu, xb, vec![leaf(0)])?;
    let x_esc = CovVtxo::new_full(&info, &emu, &[vec![xb, xs], vec![xb, xa], vec![xs, xa]], None, vec![leaf(1)], None)?;
    let x_loan = CovVtxo::new_full(&info, &emu, &[vec![xb, xl]], None, vec![leaf(2)], None)?;
    for (n, c) in [("x_old", &x_old), ("x_esc", &x_esc), ("x_loan", &x_loan)] {
        sc.rec(&format!("{n}_leaves"), exit_leaf_report(c));
    }
    let (fund_tx, cp_a) = fund_contracts(&bob, xb, &info, &pre, &[&x_old, &x_esc, &x_loan], &bob_addr).await?;
    let ft = fund_tx.compute_txid();
    let (v_old, v_esc, v_loan) = (OutPoint { txid: ft, vout: 1 }, OutPoint { txid: ft, vout: 2 }, OutPoint { txid: ft, vout: 3 });
    sc.rec("funded", format!("X_old {v_old}, X_esc {v_esc}, X_loan {v_loan}"));

    step("2. Bob pre-signs the default tx X_loan -> Lena (nLockTime = maturity height), hands it to Lena");
    let h0 = height(&regtest)?;
    let maturity_h = (h0 + 40) as u32;
    let presig = exit_tx(&x_loan, 0, v_loan, pre.contracts[2].t1.c, p2tr_of(&lena), maturity_h, &[Some(&bkp), None])?;
    let bob_sig = presig.input[0].witness.nth(1).map(|s| s.to_vec()).ok_or_else(|| anyhow!("no sig"))?;
    sc.rec("presigned_default", format!("nLockTime {maturity_h} (now {h0}), Bob's signature {} B; Lena keeps it", bob_sig.len()));

    step("3. Bob unrolls the funding tx on-chain (no operator, no emulator)");
    let t = Instant::now();
    let branch = branch_any(&bob, &regtest, v_old).await?;
    sc.rec("branch", format!("{} txs: {:?}", branch.len(), branch.iter().map(|t| t.compute_txid()).collect::<Vec<_>>()));
    let mut wt = crate::exit_watchtower::Watchtower::new(&regtest).await?;
    {
        let mut r = |k: &str, v: String| println!("  >> {k} = {v}");
        crate::checkpoint_offline::unroll(&mut wt, &regtest, &branch, &mut r).await?;
    }
    let h_conf = height(&regtest)?;
    sc.rec("unrolled", format!("funding tx {ft} confirmed at height {h_conf} in {} s", t.elapsed().as_secs()));
    let early = exit_tx(&x_esc, 1, v_esc, pre.contracts[1].t1.c, p2tr_of(&bkp), 0, &[Some(&bkp), Some(&ari)])?;
    sc.rec("neg_bob_ari_before_csv", format!("{:?}", mempool_check(&regtest, &early)));
    ensure!(mempool_check(&regtest, &early).is_err(), "exit before CSV accepted");
    let csv = x_esc.vtxo.exit_delay().to_consensus_u32() as u64;
    mine_to(&regtest, h_conf + csv)?;
    sc.rec("csv_waited", format!("{csv} blocks, height {}", height(&regtest)?));

    let mut map = vec![(pre.t2f.ph_txid, fund_tx.clone())];
    let st = sc.state.clone();
    let validate = |map: &Vec<(Txid, Transaction)>, t1: &Pre, name: &str, tx: &Transaction| -> anyhow::Result<(u64, u64)> {
        let fin = st.join(format!("{name}.rgbc"));
        let mut m = map.clone();
        m.push((t1.ph_txid, tx.clone()));
        reanchor_all(&t1.consignment, &fin, &m)?;
        let u = valid_units(&bob_rgb, &bob_online, &fin, &[], tx.compute_txid(), 1)?;
        Ok((u, std::fs::metadata(&fin)?.len()))
    };

    step("4. legacy single-key exit: Bob alone takes X_old with the precomputed T1 (the bypass)");
    let tx = exit_tx(&x_old, 0, v_old, pre.contracts[0].t1.c, p2tr_of(&bkp), 0, &[Some(&bkp)])?;
    sc.rec("legacy_bob_alone_mempool", format!("{:?}", mempool_check(&regtest, &tx)));
    regtest.send_raw(&tx)?;
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (u, sz) = validate(&map, &pre.contracts[0].t1, "x-old-bob", &tx)?;
    sc.rec("legacy_bypass", format!("Bob alone: {u} units on-chain at {}:1 (vsize {} vB, consignment {sz} B) WITHOUT any covenant outcome", tx.compute_txid(), tx.vsize()));
    ensure!(u == 300, "legacy bypass gave {u}");

    step("5. multi-party exits: Bob alone is refused by consensus");
    let n1 = exit_tx(&x_esc, 0, v_esc, pre.contracts[1].t1.c, p2tr_of(&bkp), 0, &[Some(&bkp), None])?;
    let n2 = exit_tx(&x_esc, 1, v_esc, pre.contracts[1].t1.c, p2tr_of(&bkp), 0, &[Some(&bkp), None])?;
    let n3 = exit_tx(&x_loan, 0, v_loan, pre.contracts[2].t1.c, p2tr_of(&bkp), 0, &[Some(&bkp), None])?;
    // Bob signs both slots with his own key
    let n4 = exit_tx(&x_loan, 0, v_loan, pre.contracts[2].t1.c, p2tr_of(&bkp), 0, &[Some(&bkp), Some(&bkp)])?;
    for (n, t) in [("esc_bob_alone_leaf_bob_sally", &n1), ("esc_bob_alone_leaf_bob_ari", &n2), ("loan_bob_alone", &n3), ("loan_bob_signs_both_slots", &n4)] {
        let r = mempool_check(&regtest, t);
        sc.rec(&format!("neg_{n}"), format!("{r:?}"));
        ensure!(r.is_err(), "{n} accepted");
    }

    step("6. X_loan: Lena completes Bob's pre-signed default; refused before the locktime, accepted after");
    let mut def = exit_tx(&x_loan, 0, v_loan, pre.contracts[2].t1.c, p2tr_of(&lena), maturity_h, &[None, Some(&lena)])?;
    let mut w: Vec<Vec<u8>> = def.input[0].witness.iter().map(|x| x.to_vec()).collect();
    w[1] = bob_sig.clone(); // [sig_lena, sig_bob, script, cb]
    def.input[0].witness = bitcoin::Witness::from_slice(&w);
    let r = mempool_check(&regtest, &def);
    sc.rec("neg_default_before_locktime", format!("height {}: {r:?}", height(&regtest)?));
    ensure!(r.is_err(), "default before locktime accepted");
    mine_to(&regtest, maturity_h as u64)?;
    let r = mempool_check(&regtest, &def);
    sc.rec("default_after_locktime_mempool", format!("height {}: {r:?}", height(&regtest)?));
    regtest.send_raw(&def)?;
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (u, sz) = validate(&map, &pre.contracts[2].t1, "x-loan-default", &def)?;
    sc.rec("loan_default_onchain", format!("{u} units to Lena at {}:1 (Bob offline; vsize {} vB, consignment {sz} B)", def.compute_txid(), def.vsize()));
    ensure!(u == 400, "default gave {u}");

    step("7. X_esc: Bob + Ari (arbiter-backed refund) exit on-chain");
    let tx = exit_tx(&x_esc, 1, v_esc, pre.contracts[1].t1.c, p2tr_of(&bkp), 0, &[Some(&bkp), Some(&ari)])?;
    sc.rec("esc_bob_ari_mempool", format!("{:?}", mempool_check(&regtest, &tx)));
    regtest.send_raw(&tx)?;
    regtest::mine(1)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (u, sz) = validate(&map, &pre.contracts[1].t1, "x-esc-refund", &tx)?;
    sc.rec("esc_refund_onchain", format!("{u} units to Bob at {}:1 (vsize {} vB, consignment {sz} B)", tx.compute_txid(), tx.vsize()));
    ensure!(u == 300, "escrow exit gave {u}");
    map.clear();
    let _ = cp_a;
    println!("EMU-EXIT RESULT: PASS");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Scenario emu-renew: RGB across batches.
//   A. forfeit path (what an intent-based batch renewal does to the old VTXO), plain RGB VTXO:
//      A1 a forfeit carrying an RGB commitment (arkd:rgb lane style) on cponly arkd
//      A2 a plain refresh: the forfeit is the only spend of the old VTXO; does it commit?
//      A3 the old VTXO can no longer move offchain; after the batch expiry it is swept
//   B. live migration of a lending contract (Bob borrower, Lena lender) before batch 1 expires:
//      V (batch 1, covenant: fill + collab{server,Bob,Lena} + exit{Bob,Lena})
//      hop 1: V -> G via the collab leaf, checkpoint commits T_r (explicit seal on G; G is a
//             {Bob,Lena} VTXO funded from a batch-2 VTXO, so it carries batch 2's expiry)
//      hop 2: G -> V' (fresh covenant, tree recomputed; witness-vout seal, no circularity)
//      batch 1 expires and is swept; Bob repays on V' through the emulator; Bob validates the
//      whole history genesis -> ... -> V -> G -> V' -> repay.
// ---------------------------------------------------------------------------------------------
fn sign_keys<'a>(kps: &'a [&'a bitcoin::key::Keypair]) -> impl FnOnce(&mut psbt::Input, bitcoin::secp256k1::Message) -> Result<Vec<(bitcoin::secp256k1::schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> + 'a {
    move |_, msg| {
        let secp = Secp256k1::new();
        Ok(kps.iter().map(|kp| (secp.sign_schnorr_no_aux_rand(&msg, kp), kp.x_only_public_key().0)).collect())
    }
}

/// Submit an ark tx whose inputs are all spent through multi-party leaves signed by `kps`.
async fn submit_multi(any: &ArkClient, mut ark_tx: Psbt, cps: Vec<Psbt>, kps: &[&bitcoin::key::Keypair]) -> anyhow::Result<(Txid, u128)> {
    let t = Instant::now();
    for i in 0..ark_tx.inputs.len() {
        ark_core::send::sign_ark_transaction(sign_keys(kps), &mut ark_tx, i).map_err(|e| anyhow!("sign ark: {e:?}"))?;
    }
    let txid = ark_tx.unsigned_tx.compute_txid();
    let res = any.network_client().submit_offchain_transaction_request(ark_tx.clone(), cps).await.map_err(|e| anyhow!("submit: {e:?}"))?;
    ensure!(res.signed_ark_tx.unsigned_tx.compute_txid() == txid, "arkd rebuilt the ark tx");
    let mut signed = res.signed_checkpoint_txs;
    for c in signed.iter_mut() {
        if c.inputs[0].witness_script.is_none() {
            let idx = ark_tx.unsigned_tx.input.iter().position(|i| i.previous_output.txid == c.unsigned_tx.compute_txid()).unwrap_or(0);
            c.inputs[0].witness_script = ark_tx.inputs[idx].witness_script.clone();
        }
        ark_core::send::sign_checkpoint_transaction(sign_keys(kps), c).map_err(|e| anyhow!("sign cp: {e:?}"))?;
    }
    any.finalize_offchain_tx(txid, signed).await.map_err(|e| anyhow!("finalize: {e:?}"))?;
    Ok((txid, t.elapsed().as_millis()))
}

pub async fn run_renew() -> anyhow::Result<()> {
    use crate::forfeit::{BatchOp, colored_batch, hooked_batch};
    use crate::long_offline::vtxo_info;
    use crate::swap_ni::{height, mine_to};
    use ark_core::send::OffchainTransactions;
    use rgb_lib::wallet::Outpoint as RgbOutpoint;
    const TREE_EXPIRY: u64 = 500;
    let mut sc = Scn::new("emu-renew")?;
    let regtest = Arc::new(Regtest::new());
    emulator_up("emulator:rgb", &[])?;
    let emu = emu_pubkey().await?;

    step("A0. Fred: plain RGB VTXO f0 (batch 1)");
    let (fred, _) = ark_client("fred", regtest.clone(), &sc.state).await?;
    let info = fred.server_info().await?;
    board("fred", &fred, "0.001").await?;
    let f0 = first(&fred, "fred").await?;
    let h1 = height(&regtest)?;
    let (fred_addr, _) = fred.get_offchain_address().await?;
    let (mut fred_rgb, fred_online) = bl(|| rgb_wallet("fred", &sc.state))?;
    let fasset = onchain_genesis_jump_in(&regtest, &mut fred_rgb, &fred_online, f0, &sc.state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let fcid = ContractId::from_str(&fasset).map_err(|e| anyhow!("{e}"))?;
    sc.rec("f0", format!("{f0} at height {h1}, expires_at {:?}", vtxo_info(f0).await.map(|x| x.expires_at)));

    step("A1. refresh f0 with an RGB commitment in the forfeit (arkd:rgb lane style) on cponly arkd");
    let r = colored_batch("fred-colored", &fred, &fred_rgb, fcid, f0, ISSUE_AMOUNT, BatchOp::Refresh(fred_addr.clone())).await;
    sc.rec("a1_colored_forfeit_on_cponly", format!("{:?}", r.as_ref().map(|(c, fc)| format!("ACCEPTED commitment {c}, forfeit {}", fc.forfeit_tx.compute_txid())).map_err(|e| format!("{e:#}").chars().take(400).collect::<String>())));
    let a1_accepted = r.is_ok();

    let mut forfeit_seen: Option<Transaction> = None;
    if !a1_accepted {
        step("A2. plain refresh of f0 (what any intent renewal does): capture the forfeit");
        let slot = &mut forfeit_seen;
        let mut body = |_: &ForfeitHookContext<'_>, p: &mut Psbt| -> anyhow::Result<()> { *slot = Some(p.unsigned_tx.clone()); Ok(()) };
        let c = hooked_batch("fred-plain", &fred, f0, BatchOp::Refresh(fred_addr.clone()), &mut body, 3).await?;
        let ff = forfeit_seen.clone().ok_or_else(|| anyhow!("no forfeit captured"))?;
        let oprets = ff.output.iter().filter(|o| o.script_pubkey.is_op_return()).count();
        sc.rec("a2_plain_refresh", format!("commitment {c}; forfeit {} inputs {:?} outputs {:?}; OP_RETURN outputs: {oprets}", ff.compute_txid(), ff.input.iter().map(|i| i.previous_output).collect::<Vec<_>>(), ff.output.iter().map(|o| (o.value.to_sat(), o.script_pubkey.to_hex_string().chars().take(12).collect::<String>())).collect::<Vec<_>>()));
        ensure!(ff.input.iter().any(|i| i.previous_output == f0), "forfeit does not spend f0");
        let vi = vtxo_info(f0).await;
        sc.rec("a2_f0_after_refresh", format!("{:?}", vi.as_ref().map(|x| (x.is_spent, x.spent_by, x.settled_by))));
        step("A3. the old VTXO cannot carry the asset anywhere else offchain");
        let hop = fred.resolve_selected_send_inputs(&[f0]).await;
        let r: anyhow::Result<()> = match hop {
            Err(e) => Err(anyhow!("resolve: {e}")),
            Ok(mut v) => {
                let inp = v.remove(0).with_checkpoint_op_return(Some([7u8; 32]));
                let b = build_offchain_transactions(&[SendReceiver::bitcoin(fred_addr.clone(), inp.amount())], &fred_addr, &[inp], &info)?;
                fred.network_client().submit_offchain_transaction_request(b.ark_tx, b.checkpoint_txs).await.map(|_| ()).map_err(|e| anyhow!("{e:?}"))
            }
        };
        sc.rec("a3_offchain_hop_from_f0", format!("{:?}", r.as_ref().map_err(|e| format!("{e:#}").chars().take(300).collect::<String>())));
        ensure!(r.is_err(), "arkd accepted a spend of a forfeited VTXO");
        let bal = { use rgb_lib::wallet::RgbWalletOpsOffline; bl(|| fred_rgb.get_asset_balance(fasset.clone()))? };
        sc.rec("a3_fred_rgb_balance", format!("{bal:?} (wallet view only: the failed A1 attempt already consumed a never-signed transition locally; the evidence is the forfeit above and the refused hop)"));
    }

    step("B0. Bob (borrower, asset) and Lena (lender) clients");
    let (bob, bkp) = keypair_client("bob", regtest.clone(), &sc.state).await?;
    let (lena, lkp) = keypair_client("lena", regtest.clone(), &sc.state).await?;
    board("bob", &bob, "0.001").await?;
    let b0 = first(&bob, "bob").await?;
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (lena_addr, _) = lena.get_offchain_address().await?;
    let (xb, xl) = (bkp.x_only_public_key().0, lkp.x_only_public_key().0);
    let oracle = new_keypair();
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &sc.state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut bob_rgb, &bob_online, b0, &sc.state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    let loan_leaves = |c: &PreContract, maturity: u64| vec![
        Leaf { c1: c.t1.c, c2: c.t2[0].c, pay_key: out_key(&lena_addr), pay_min: REPAY_SAT, rgb_key: out_key(&bob_addr), not_after: maturity, ..Default::default() },
        Leaf { c1: c.t1.c, c2: c.t2[1].c, rgb_key: out_key(&lena_addr), auth_key: oracle.x_only_public_key().0.serialize(), price_below: THRESHOLD, ..Default::default() },
        Leaf { c1: c.t1.c, c2: c.t2[2].c, rgb_key: out_key(&lena_addr), not_before: maturity, ..Default::default() },
    ];
    let specs = vec![(ISSUE_AMOUNT, vec![vec![(2u32, ISSUE_AMOUNT)]; 3])];

    step("B1. loan V in batch 1 (leaves: fill, collab {server,Bob,Lena}, exit {Bob,Lena})");
    let t = Instant::now();
    let pre = precompute(&bob, &info, &bob_rgb, cid, b0, &bob_addr, &specs, &sc.state, "loan1").await?;
    let cov = CovVtxo::new_full(&info, &emu, &[vec![xb, xl]], Some(&[xb, xl]), loan_leaves(&pre.contracts[0], unix_now() + 86_400), None)?;
    let pre_ms = t.elapsed().as_millis();
    let (fund_tx, cp_a) = fund_contracts(&bob, xb, &info, &pre, &[&cov], &bob_addr).await?;
    let v = OutPoint { txid: fund_tx.compute_txid(), vout: 1 };
    let hv = height(&regtest)?;
    sc.rec("b1_loan", format!("V {v} (precompute {pre_ms} ms), {}; expires_at {:?}", exit_leaf_report(&cov), vtxo_info(v).await.map(|x| x.expires_at)));

    step("B2. 120 blocks later: Lena boards (batch 2) and funds G = {server,Bob,Lena} collab + exit {Bob,Lena}");
    regtest::mine(120)?;
    board("lena", &lena, "0.001").await?;
    let l0 = first(&lena, "lena").await?;
    let server = info.signer_pk.x_only_public_key().0;
    let g_collab = nofn_script(&[server, xb, xl]);
    let g_exit = csv_multisig_script(info.unilateral_exit_delay, &[xb, xl]);
    let g_vtxo = ark_core::Vtxo::new_with_custom_scripts(&Secp256k1::new(), server, xb, vec![g_collab.clone(), g_exit.clone()], info.unilateral_exit_delay, Network::Regtest)?;
    let g_addr = g_vtxo.to_ark_address();
    const G_SAT: u64 = 10_000;
    let l_in = lena.resolve_selected_send_inputs(&[l0]).await?.remove(0);
    let gb = build_offchain_transactions(&[SendReceiver::bitcoin(g_addr.clone(), Amount::from_sat(G_SAT))], &lena_addr, &[l_in], &info)?;
    ensure!(gb.ark_tx.unsigned_tx.output[0].script_pubkey == g_addr.to_p2tr_script_pubkey(), "G not at output 0");
    let g = OutPoint { txid: gb.ark_tx.unsigned_tx.compute_txid(), vout: 0 };
    let pend = lena.submit_built_offchain_send(gb.ark_tx, gb.checkpoint_txs, lena.get_offchain_address().await?.1.owner_pk()).await.map_err(|e| anyhow!("{e:?}"))?;
    lena.sign_and_finalize_pending_tx(pend).await.map_err(|e| anyhow!("{e:?}"))?;
    let (ev, eg) = (vtxo_info(v).await.map(|x| x.expires_at), vtxo_info(g).await.map(|x| x.expires_at));
    sc.rec("b2_g", format!("G {g} ({G_SAT} sat) from Lena's batch-2 VTXO {l0}; expires_at V {ev:?} vs G {eg:?}"));
    ensure!(eg > ev, "G does not expire later than V");

    step("B3. hop 1: V -> G through the collab leaf (Bob + Lena sign; no emulator), T_r in V's checkpoint");
    let t = Instant::now();
    let v_ph = OutPoint { txid: pre.t2f.ph_txid, vout: 1 };
    let mut ph = placeholder(&[v_ph], 2, 0xe1);
    let t_r = precolor(&bob_rgb, &mut ph, cid, vec![(SealTarget::Explicit(RgbOutpoint { txid: g.txid.to_string(), vout: g.vout }), ISSUE_AMOUNT, 9901)], sc.state.join("renew-tr.rgbc"))?;
    let v_in = cov.input_leaf(cov.collab.as_ref().unwrap(), V_SAT, v)?.with_checkpoint_op_return(Some(t_r.c));
    let OffchainTransactions { ark_tx: hop1, checkpoint_txs: hop1_cps } = build_offchain_transactions(&[SendReceiver::bitcoin(bob_addr.clone(), Amount::from_sat(V_SAT))], &bob_addr, &[v_in], &info)?;
    let cp_hop = hop1_cps[0].unsigned_tx.clone();
    let (hop1_txid, hop1_ms) = submit_multi(&bob, hop1, hop1_cps, &[&bkp, &lkp]).await?;
    sc.rec("b3_hop1", format!("ark tx {hop1_txid}, checkpoint {} commits T_r (consignment {} B); submit+finalize {hop1_ms} ms; T_r computed in {} ms total", cp_hop.compute_txid(), std::fs::metadata(&t_r.consignment)?.len(), t.elapsed().as_millis()));
    sc.rec("b3_v_after_hop1", spent(v).await);
    let mut map = vec![(pre.t2f.ph_txid, fund_tx.clone()), (t_r.ph_txid, cp_hop.clone())];
    let mut offchain = vec![cp_a.compute_txid().to_string(), fund_tx.compute_txid().to_string(), cp_hop.compute_txid().to_string(), hop1_txid.to_string()];
    let fin_g = sc.state.join("renew-on-g.rgbc");
    reanchor_all(&t_r.consignment, &fin_g, &map)?;
    let ug = valid_units(&bob_rgb, &bob_online, &fin_g, &offchain, g.txid, g.vout)?;
    sc.rec("b3_units_on_g", ug.to_string());
    ensure!(ug == ISSUE_AMOUNT, "G holds {ug}");

    step("B4. hop 2: G -> V' (fresh covenant, tree recomputed; witness-vout funding as in 1.1)");
    let t = Instant::now();
    let g_in = VtxoInput::new(g_collab.clone(), None, g_vtxo.get_spend_info(g_collab.clone())?, vec![g_collab.clone(), g_exit.clone()], g_addr.to_p2tr_script_pubkey(), Amount::from_sat(G_SAT), g, vec![]);
    let pre2 = precompute_in(&info, &bob_rgb, cid, g_in.clone(), &bob_addr, &specs, &sc.state, "loan2")?;
    let maturity2 = unix_now() + 86_400;
    let cov2 = CovVtxo::new_full(&info, &emu, &[vec![xb, xl]], Some(&[xb, xl]), loan_leaves(&pre2.contracts[0], maturity2), None)?;
    let pre2_ms = t.elapsed().as_millis();
    let built = build_offchain_transactions(&[SendReceiver::bitcoin(cov2.addr.clone(), Amount::from_sat(V_SAT))], &bob_addr, &[g_in.with_checkpoint_op_return(Some(pre2.t1a.c))], &info)?;
    let mut ark = built.ark_tx.clone();
    ensure!(ark.unsigned_tx.output[1].script_pubkey == bob_addr.to_p2tr_script_pubkey(), "fund2 layout");
    ark.unsigned_tx.output[1].value -= Amount::from_sat(OPRET_SAT);
    ark.unsigned_tx.output.insert(0, TxOut { value: Amount::from_sat(OPRET_SAT), script_pubkey: ScriptBuf::new_op_return(pre2.t2f.c) });
    ark.outputs.insert(0, psbt::Output::default());
    let cp_g = built.checkpoint_txs[0].unsigned_tx.clone();
    ensure!(cp_g.compute_txid() == pre2.t1a.ph_txid, "G's checkpoint differs from the precolored one");
    let fund2 = ark.unsigned_tx.clone();
    let (fund2_txid, hop2_ms) = submit_multi(&bob, ark, built.checkpoint_txs, &[&bkp, &lkp]).await?;
    let v2 = OutPoint { txid: fund2_txid, vout: 1 };
    let ev2 = vtxo_info(v2).await.map(|x| x.expires_at);
    sc.rec("b4_hop2", format!("V' {v2} ({}); precompute {pre2_ms} ms, submit+finalize {hop2_ms} ms; expires_at V' {ev2:?} (G {eg:?}, old V {ev:?})", exit_leaf_report(&cov2)));
    ensure!(ev2 == eg, "V' does not carry G's expiry");
    map.push((pre2.t2f.ph_txid, fund2.clone()));
    offchain.extend([cp_g.compute_txid().to_string(), fund2_txid.to_string()]);

    step("B5. batch 1 expires: mine past it, wait for the sweep");
    mine_to(&regtest, hv.max(h1) + TREE_EXPIRY + 20)?;
    let mut swept = None;
    for _ in 0..40 {
        swept = vtxo_info(OutPoint { txid: hop1_txid, vout: 0 }).await;
        if swept.as_ref().is_some_and(|x| x.is_swept) { break; }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    sc.rec("b5_batch1_after_expiry", format!("height {}: hop-1 output (old chain) swept {:?}; f0 {:?}; V' {:?}", height(&regtest)?, swept.as_ref().map(|x| x.is_swept), vtxo_info(f0).await.map(|x| (x.is_spent, x.is_swept)), vtxo_info(v2).await.map(|x| (x.is_spent, x.is_swept))));

    step("B6. the contract continues on V': Bob (fresh batch-3 BTC) repays through the emulator");
    board("bob", &bob, "0.001").await?;
    let mut bob_btc_ok = None;
    for o in bob.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().map(|x| x.vtxo().outpoint) {
        if let Some(i) = vtxo_info(o).await { if !i.is_swept && !i.is_spent && i.amount.to_sat() >= 50_000 { bob_btc_ok = Some(o); break; } }
    }
    let bob_btc = bob_btc_ok.ok_or_else(|| anyhow!("no fresh bob vtxo"))?;
    let mut ctx = Ctx { state: sc.state.clone(), map, offchain };
    let neg = cov_spend(&info, &cov2, v2, V_SAT, 0, None, vec![
        TxOut { value: Amount::from_sat(REPAY_SAT - 1), script_pubkey: lena_addr.to_p2tr_script_pubkey() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: bob_addr.to_p2tr_script_pubkey() },
    ], Some((&bob, bob_btc)), &bob_addr, &[], &[]).await?;
    sc.rec("b6_neg_repay_underpaid_on_v2", ctx.refuse(&bob, neg).await?);
    let s = cov_spend(&info, &cov2, v2, V_SAT, 0, None, vec![
        TxOut { value: Amount::from_sat(REPAY_SAT), script_pubkey: lena_addr.to_p2tr_script_pubkey() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: bob_addr.to_p2tr_script_pubkey() },
    ], Some((&bob, bob_btc)), &bob_addr, &[], &[]).await?;
    let (fin, txid, ms) = ctx.settle(&bob, s, &pre2.contracts[0].t1, &pre2.contracts[0].t2[0], "v2-repay").await?;
    wait_vtxo(&bob, OutPoint { txid, vout: 2 }, "bob").await?;
    let tv = Instant::now();
    let u = valid_units(&bob_rgb, &bob_online, &fin, &ctx.offchain, txid, 2)?;
    let n_bundles = RgbTransfer::load_file(&fin).map(|c| c.bundles.len()).unwrap_or(0);
    sc.rec("b6_repaid_after_renewal", format!("{u} units back to Bob at {txid}:2 (Lena paid {REPAY_SAT} sat), emulator {ms} ms; full-history consignment {} B, {n_bundles} bundles, validated in {} ms", std::fs::metadata(&fin)?.len(), tv.elapsed().as_millis()));
    ensure!(u == ISSUE_AMOUNT, "after renewal Bob validates {u}");
    let _ = fred_online;
    println!("EMU-RENEW RESULT: PASS");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// arkd:rgb-master lane (checkpoint, forfeit and ark-tx data OP_RETURN): covenant renewal through a batch
// ---------------------------------------------------------------------------------------------

/// Key provider holding several keys: one client signs every leaf key it holds (both parties of
/// a collaborative leaf when they co-sign through one renewal session).
struct MultiKey(Vec<bitcoin::key::Keypair>);

impl ark_client::key_provider::KeyProvider for MultiKey {
    fn get_next_keypair(&self, _: ark_client::key_provider::KeypairIndex) -> Result<bitcoin::key::Keypair, ark_client::Error> {
        Ok(self.0[0])
    }
    fn get_keypair_for_path(&self, _: &[u32]) -> Result<bitcoin::key::Keypair, ark_client::Error> {
        Ok(self.0[0])
    }
    fn get_keypair_for_pk(&self, pk: &XOnlyPublicKey) -> Result<bitcoin::key::Keypair, ark_client::Error> {
        match self.0.iter().find(|k| k.x_only_public_key().0 == *pk) {
            Some(k) => Ok(*k),
            None => ark_client::key_provider::StaticKeyProvider::new(self.0[0]).get_keypair_for_pk(pk),
        }
    }
    fn get_cached_pks(&self) -> Result<Vec<XOnlyPublicKey>, ark_client::Error> {
        Ok(self.0.iter().map(|k| k.x_only_public_key().0).collect())
    }
}

async fn multikey_client(name: &str, rt: Arc<Regtest>, kps: Vec<bitcoin::key::Keypair>) -> anyhow::Result<ArkClient> {
    use ark_client::{InMemorySwapStorage, OfflineClient, OfflineClientConfig};
    let mut sk = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut sk);
    let wkp = bitcoin::key::Keypair::from_secret_key(&Secp256k1::new(), &bitcoin::secp256k1::SecretKey::from_slice(&sk)?);
    let wallet = Arc::new(ark_bdk_wallet::Wallet::new(wkp, Network::Regtest, "http://localhost:3000/api")?);
    OfflineClient::with_key_provider(
        OfflineClientConfig { ark_server_url: "http://localhost:7070".into(), boltz_url: "http://localhost:9069".into(), ..Default::default() },
        Arc::new(MultiKey(kps)),
        rt,
        wallet,
        Arc::new(InMemorySwapStorage::default()),
    )
    .connect_with_retries(5)
    .await
    .map_err(|e| anyhow!("{name}: connect: {e:?}"))
}

/// A live covenant VTXO renewed through the batch (forfeit path): the forfeit carries T_f
/// (V -> G' in the new VTXO tree, explicit seal), then G' re-funds a fresh covenant V' (tree
/// recomputed), batch 1 is swept, and the contract completes an outcome on V'.
pub async fn run_renew_batch() -> anyhow::Result<()> {
    use crate::long_offline::vtxo_info;
    use crate::swap_ni::{height, mine_to};
    use rgb_lib::wallet::Outpoint as RgbOutpoint;
    const TREE_EXPIRY: u64 = 500;
    let mut sc = Scn::new("emu-renew-batch")?;
    let regtest = Arc::new(Regtest::new());
    emulator_up("emulator:rgb", &[])?;
    let emu = emu_pubkey().await?;

    step("R0. Bob (borrower, asset) and Lena (lender)");
    let (bob, bkp) = keypair_client("bob", regtest.clone(), &sc.state).await?;
    let (lena, lkp) = keypair_client("lena", regtest.clone(), &sc.state).await?;
    let info = bob.server_info().await?;
    board("bob", &bob, "0.001").await?;
    let b0 = first(&bob, "bob").await?;
    let h1 = height(&regtest)?;
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let (lena_addr, _) = lena.get_offchain_address().await?;
    let (xb, xl) = (bkp.x_only_public_key().0, lkp.x_only_public_key().0);
    let oracle = new_keypair();
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &sc.state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut bob_rgb, &bob_online, b0, &sc.state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    let loan_leaves = |c: &PreContract, maturity: u64| vec![
        Leaf { c1: c.t1.c, c2: c.t2[0].c, pay_key: out_key(&lena_addr), pay_min: REPAY_SAT, rgb_key: out_key(&bob_addr), not_after: maturity, ..Default::default() },
        Leaf { c1: c.t1.c, c2: c.t2[1].c, rgb_key: out_key(&lena_addr), auth_key: oracle.x_only_public_key().0.serialize(), price_below: THRESHOLD, ..Default::default() },
        Leaf { c1: c.t1.c, c2: c.t2[2].c, rgb_key: out_key(&lena_addr), not_before: maturity, ..Default::default() },
    ];
    let specs = vec![(ISSUE_AMOUNT, vec![vec![(2u32, ISSUE_AMOUNT)]; 3])];

    step("R1. loan V in batch 1 (leaves: fill, collab {server,Bob,Lena}, exit {Bob,Lena})");
    let t = Instant::now();
    let pre = precompute(&bob, &info, &bob_rgb, cid, b0, &bob_addr, &specs, &sc.state, "loan1").await?;
    let cov = CovVtxo::new_full(&info, &emu, &[vec![xb, xl]], Some(&[xb, xl]), loan_leaves(&pre.contracts[0], unix_now() + 86_400), None)?;
    let pre_ms = t.elapsed().as_millis();
    let (fund_tx, cp_a) = fund_contracts(&bob, xb, &info, &pre, &[&cov], &bob_addr).await?;
    let v = OutPoint { txid: fund_tx.compute_txid(), vout: 1 };
    let hv = height(&regtest)?;
    let ev = vtxo_info(v).await.map(|x| x.expires_at);
    sc.rec("r1_loan", format!("V {v} (precompute {pre_ms} ms), {}; expires_at {ev:?}", exit_leaf_report(&cov)));

    step("R2. 120 blocks later: Bob boards fresh sats (batch 2) for the renewal output's bitcoin");
    regtest::mine(120)?;
    board("bob", &bob, "0.001").await?;
    let mut b1 = None;
    for o in bob.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().map(|x| x.vtxo().outpoint) {
        if o != b0 { if let Some(i) = vtxo_info(o).await { if !i.is_swept && !i.is_spent && i.amount.to_sat() >= 50_000 { b1 = Some((o, i.amount)); break; } } }
    }
    let (b1, b1_amt) = b1.ok_or_else(|| anyhow!("no fresh bob vtxo"))?;
    let server = info.signer_pk.x_only_public_key().0;
    let g_collab = nofn_script(&[server, xb, xl]);
    let g_exit = csv_multisig_script(info.unilateral_exit_delay, &[xb, xl]);
    let g_vtxo = ark_core::Vtxo::new_with_custom_scripts(&Secp256k1::new(), server, xb, vec![g_collab.clone(), g_exit.clone()], info.unilateral_exit_delay, Network::Regtest)?;
    let g_addr = g_vtxo.to_ark_address();
    let g_spk = g_addr.to_p2tr_script_pubkey();
    let g_amt = Amount::from_sat(V_SAT) + b1_amt;
    let collab = cov.collab.clone().ok_or_else(|| anyhow!("no collab leaf"))?;
    let v_input = ark_core::intent::Input::new(
        v, info.unilateral_exit_delay, None,
        TxOut { value: Amount::from_sat(V_SAT), script_pubkey: cov.addr.to_p2tr_script_pubkey() },
        cov.scripts(), (collab.clone(), cov.vtxo.get_spend_info(collab.clone())?), false, false, vec![],
    );
    let b1_inputs = bob.rgb_settleable_inputs(&[b1]).await.map_err(|e| anyhow!("{e:?}"))?;
    sc.rec("r2_inputs", format!("V {v} via collab leaf + Bob's batch-2 VTXO {b1} ({b1_amt}); renewal output G' = {{server,Bob,Lena}} collab + exit {{Bob,Lena}}, {g_amt}"));

    step("R2n. negative: Bob alone (holding only his key) tries the same batch renewal");
    let bob_only = multikey_client("bob-only", regtest.clone(), vec![bkp]).await?;
    let neg = {
        let mut hook = |ctx: &ForfeitHookContext<'_>, _: &mut Psbt| -> Result<(), ark_core::Error> {
            if ctx.vtxo == v { Err(ark_core::Error::ad_hoc("negative test reached the forfeit step: abort")) } else { Ok(()) }
        };
        let mut rng = rand::thread_rng();
        tokio::time::timeout(Duration::from_secs(60), bob_only.settle_inputs_with_forfeit_hook(&mut rng, [vec![v_input.clone()], b1_inputs.clone()].concat(), g_addr.clone(), g_amt, &mut hook)).await
    };
    let neg_s = match &neg { Err(_) => "timeout (no batch joined)".to_string(), Ok(Ok(c)) => format!("ACCEPTED commitment {c}"), Ok(Err(e)) => format!("refused: {}", format!("{e:?}").chars().take(300).collect::<String>()) };
    sc.rec("r2n_bob_alone_batch_renewal", neg_s);
    ensure!(!matches!(neg, Ok(Ok(_))), "Bob alone renewed the covenant VTXO");
    let _ = bob.delete_intents(&[b1]).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    step("R3. batch renewal: one session holding Bob's and Lena's keys signs intent + forfeits; the forfeit of V carries T_f (V -> G', explicit seal on the new tree leaf)");
    let renewer = multikey_client("renewer", regtest.clone(), vec![bkp, lkp]).await?;
    let v_ph = OutPoint { txid: pre.t2f.ph_txid, vout: 1 };
    let tf_file = sc.state.join("renew-tf.rgbc");
    let mut got: Option<(Pre, Transaction, OutPoint, u128)> = None;
    let t = Instant::now();
    let mut commitment = None;
    let mut last_err = String::new();
    for attempt in 0..3 {
        let slot = &mut got;
        let mut hook = |ctx: &ForfeitHookContext<'_>, p: &mut Psbt| -> Result<(), ark_core::Error> {
            if ctx.vtxo != v { return Ok(()); }
            let r = (|| -> anyhow::Result<()> {
                let tc = Instant::now();
                let target = ctx.vtxo_tree_leaves.iter()
                    .find_map(|tx| tx.output.iter().position(|o| o.script_pubkey == g_spk).map(|i| OutPoint { txid: tx.compute_txid(), vout: i as u32 }))
                    .ok_or_else(|| anyhow!("G' not in the tree leaves"))?;
                ensure!(!p.unsigned_tx.output.iter().any(|o| o.script_pubkey.is_op_return()), "forfeit already has an OP_RETURN");
                let mut ph = placeholder(&[v_ph], 2, 0xf1);
                let tf = precolor(&bob_rgb, &mut ph, cid, vec![(SealTarget::Explicit(RgbOutpoint { txid: target.txid.to_string(), vout: target.vout }), ISSUE_AMOUNT, 9911)], tf_file.clone())?;
                p.unsigned_tx.output.insert(0, TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return(tf.c) });
                p.outputs.insert(0, psbt::Output::default());
                println!("[renewer] forfeit {} commits T_f: {v} -> {target}", p.unsigned_tx.compute_txid());
                *slot = Some((tf, p.unsigned_tx.clone(), target, tc.elapsed().as_millis()));
                Ok(())
            })();
            r.map_err(|e| ark_core::Error::ad_hoc(format!("forfeit hook: {e:#}")))
        };
        let mut rng = rand::thread_rng();
        match tokio::time::timeout(Duration::from_secs(120), renewer.settle_inputs_with_forfeit_hook(&mut rng, [vec![v_input.clone()], b1_inputs.clone()].concat(), g_addr.clone(), g_amt, &mut hook)).await {
            Ok(Ok(c)) => { commitment = Some(c); break; }
            Ok(Err(e)) => { last_err = format!("{e:?}"); println!("[renewer] attempt {attempt} failed: {last_err}"); }
            Err(_) => { last_err = "timeout".into(); println!("[renewer] attempt {attempt} timed out"); }
        }
        ensure!(got.is_none(), "a failed attempt already consumed T_f locally: {last_err}");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let commitment = commitment.ok_or_else(|| anyhow!("batch renewal failed: {last_err}"))?;
    let batch_ms = t.elapsed().as_millis();
    let (tf, forfeit, gp, tf_ms) = got.ok_or_else(|| anyhow!("forfeit hook never ran for V"))?;
    let ftxid = forfeit.compute_txid();
    sc.rec("r3_batch", format!("commitment {commitment}; forfeit {ftxid} outputs {:?}; G' {gp}; T_f computed in {tf_ms} ms (consignment {} B); session {batch_ms} ms",
        forfeit.output.iter().map(|o| (o.value.to_sat(), o.script_pubkey.to_hex_string().chars().take(12).collect::<String>())).collect::<Vec<_>>(),
        std::fs::metadata(&tf.consignment)?.len()));
    let fids = renewer.network_client().get_forfeit_txids(commitment.to_string()).await.map_err(|e| anyhow!("GetForfeitTxs: {e:?}"))?;
    sc.rec("r3_forfeit_attested", format!("forfeit among GetForfeitTxs({commitment}): {} ({} forfeits)", fids.contains(&ftxid.to_string()), fids.len()));
    ensure!(fids.contains(&ftxid.to_string()), "forfeit not stored by arkd");
    sc.rec("r3_v_after", format!("{:?}", vtxo_info(v).await.map(|x| (x.is_spent, x.spent_by, x.settled_by))));
    let mut seen = false;
    for _ in 0..30 { if vtxo_info(gp).await.is_some() { seen = true; break; } tokio::time::sleep(Duration::from_secs(2)).await; }
    ensure!(seen, "G' never indexed");
    let eg = vtxo_info(gp).await.map(|x| x.expires_at);
    sc.rec("r3_expiry", format!("V {ev:?} -> G' {eg:?}"));
    ensure!(eg > ev, "G' does not expire later than V");
    let mut map = vec![(pre.t2f.ph_txid, fund_tx.clone()), (tf.ph_txid, forfeit.clone())];
    let mut offchain = vec![cp_a.compute_txid().to_string(), fund_tx.compute_txid().to_string(), ftxid.to_string()];
    let fin_g = sc.state.join("renew-on-gp.rgbc");
    reanchor_all(&tf.consignment, &fin_g, &map)?;
    let tv = Instant::now();
    let ug = valid_units(&bob_rgb, &bob_online, &fin_g, &offchain, gp.txid, gp.vout)?;
    sc.rec("r3_units_on_gp", format!("{ug} (validated in {} ms, consignment {} B)", tv.elapsed().as_millis(), std::fs::metadata(&fin_g)?.len()));
    ensure!(ug == ISSUE_AMOUNT, "G' holds {ug}");

    step("R4. G' -> V' (fresh covenant, tree recomputed; witness-vout funding as in 1.1)");
    let t = Instant::now();
    let g_in = VtxoInput::new(g_collab.clone(), None, g_vtxo.get_spend_info(g_collab.clone())?, vec![g_collab.clone(), g_exit.clone()], g_spk.clone(), g_amt, gp, vec![]);
    let pre2 = precompute_in(&info, &bob_rgb, cid, g_in.clone(), &bob_addr, &specs, &sc.state, "loan2")?;
    let cov2 = CovVtxo::new_full(&info, &emu, &[vec![xb, xl]], Some(&[xb, xl]), loan_leaves(&pre2.contracts[0], unix_now() + 86_400), None)?;
    let pre2_ms = t.elapsed().as_millis();
    let built = build_offchain_transactions(&[SendReceiver::bitcoin(cov2.addr.clone(), Amount::from_sat(V_SAT))], &bob_addr, &[g_in.with_checkpoint_op_return(Some(pre2.t1a.c))], &info)?;
    let mut ark = built.ark_tx.clone();
    ensure!(ark.unsigned_tx.output[1].script_pubkey == bob_addr.to_p2tr_script_pubkey(), "fund2 layout");
    ark.unsigned_tx.output[1].value -= Amount::from_sat(OPRET_SAT);
    ark.unsigned_tx.output.insert(0, TxOut { value: Amount::from_sat(OPRET_SAT), script_pubkey: ScriptBuf::new_op_return(pre2.t2f.c) });
    ark.outputs.insert(0, psbt::Output::default());
    let cp_g = built.checkpoint_txs[0].unsigned_tx.clone();
    ensure!(cp_g.compute_txid() == pre2.t1a.ph_txid, "G''s checkpoint differs from the precolored one");
    let fund2 = ark.unsigned_tx.clone();
    let (fund2_txid, hop2_ms) = submit_multi(&bob, ark, built.checkpoint_txs, &[&bkp, &lkp]).await?;
    let v2 = OutPoint { txid: fund2_txid, vout: 1 };
    let ev2 = vtxo_info(v2).await.map(|x| x.expires_at);
    sc.rec("r4_hop", format!("V' {v2} ({}); recompute {pre2_ms} ms, submit+finalize {hop2_ms} ms; expires_at V' {ev2:?} (G' {eg:?})", exit_leaf_report(&cov2)));
    map.push((pre2.t2f.ph_txid, fund2.clone()));
    offchain.extend([cp_g.compute_txid().to_string(), fund2_txid.to_string()]);

    step("R5. batch 1 expires: mine past it, wait for the sweep");
    mine_to(&regtest, hv.max(h1) + TREE_EXPIRY + 20)?;
    let probe = OutPoint { txid: fund_tx.compute_txid(), vout: 2 };
    let mut swept = None;
    for _ in 0..40 {
        swept = vtxo_info(v).await;
        if swept.as_ref().is_some_and(|x| x.is_swept) { break; }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    sc.rec("r5_after_expiry", format!("height {}: V (forfeited) swept {:?}; fund-tx change {:?}; V' {:?}", height(&regtest)?, swept.as_ref().map(|x| x.is_swept), vtxo_info(probe).await.map(|x| (x.is_spent, x.is_swept)), vtxo_info(v2).await.map(|x| (x.is_spent, x.is_swept))));

    step("R6. the contract continues on V': Bob repays through the emulator");
    board("bob", &bob, "0.001").await?;
    let mut bob_btc = None;
    for o in bob.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().map(|x| x.vtxo().outpoint) {
        if let Some(i) = vtxo_info(o).await { if !i.is_swept && !i.is_spent && i.amount.to_sat() >= 50_000 { bob_btc = Some(o); break; } }
    }
    let bob_btc = bob_btc.ok_or_else(|| anyhow!("no fresh bob vtxo"))?;
    let mut ctx = Ctx { state: sc.state.clone(), map, offchain };
    let neg = cov_spend(&info, &cov2, v2, V_SAT, 0, None, vec![
        TxOut { value: Amount::from_sat(REPAY_SAT - 1), script_pubkey: lena_addr.to_p2tr_script_pubkey() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: bob_addr.to_p2tr_script_pubkey() },
    ], Some((&bob, bob_btc)), &bob_addr, &[], &[]).await?;
    sc.rec("r6_neg_repay_underpaid_on_v2", ctx.refuse(&bob, neg).await?);
    let s = cov_spend(&info, &cov2, v2, V_SAT, 0, None, vec![
        TxOut { value: Amount::from_sat(REPAY_SAT), script_pubkey: lena_addr.to_p2tr_script_pubkey() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: bob_addr.to_p2tr_script_pubkey() },
    ], Some((&bob, bob_btc)), &bob_addr, &[], &[]).await?;
    let (fin, txid, ms) = ctx.settle(&bob, s, &pre2.contracts[0].t1, &pre2.contracts[0].t2[0], "v2-repay").await?;
    wait_vtxo(&bob, OutPoint { txid, vout: 2 }, "bob").await?;
    let tv = Instant::now();
    let u = valid_units(&bob_rgb, &bob_online, &fin, &ctx.offchain, txid, 2)?;
    let n_bundles = RgbTransfer::load_file(&fin).map(|c| c.bundles.len()).unwrap_or(0);
    sc.rec("r6_repaid_after_batch_renewal", format!("{u} units back to Bob at {txid}:2 (Lena paid {REPAY_SAT} sat), emulator {ms} ms; full-history consignment {} B, {n_bundles} bundles, validated in {} ms", std::fs::metadata(&fin)?.len(), tv.elapsed().as_millis()));
    ensure!(u == ISSUE_AMOUNT, "after renewal Bob validates {u}");
    println!("EMU-RENEW-BATCH RESULT: PASS");
    Ok(())
}

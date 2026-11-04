//! Scenario `swap-emu`: fixed-price sale of an RGB asset enforced by an Arkade-Script covenant,
//! co-signed by the Arkade emulator (patches/emulator.patch, image emulator:rgb) on arkd cponly.
//!
//! Offer VTXO V (holds the maker's asset), taproot with three leaves:
//!   fill   = <server> CHECKSIGVERIFY <emu tweaked by covenant> CHECKSIG
//!   cancel = <H> CLTV DROP <maker> CHECKSIGVERIFY <server> CHECKSIG
//!   exit   = <exit delay> CSV DROP <maker> CHECKSIG                       (unilateral, maker only)
//! The covenant (tools/emucov, 122 B) is run by the emulator on the ark tx and checks, with the
//! taker-supplied witness [makerSig, C1, C2, rawCheckpoint]: the spent checkpoint is rawCheckpoint
//! (vout 1) and its output 0 = OP_RETURN C1; ark output 0 = OP_RETURN C2; the maker signed
//! sha256(C1||C2||V outpoint) (BIP340, CHECKSIGFROMSTACK; V read from the checkpoint's input); ark output 1 pays >= price to the maker's script.
//! C1 = T1 (V -> SOLD on CP:1, change to the maker's spare VTXO C), C2 = T2 (CP:1 -> SOLD on the
//! taker's vout 2). The maker signs (C1, C2) after funding V (C1/C2 depend on V's outpoint, V's
//! address depends on the script, so they cannot be pinned in the leaf), publishes and goes offline.
use crate::checkpoint_renew::checkpoint_hop;
use crate::offchain_transfer::{outs, wait_vtxo};
use crate::proof::{add_vtxos, arkd_down, build_proof, file_len, import, operators, proof_path};
use crate::swap_ni::{first, height, keypair_client, mine_to, reanchor, units};
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bare_opret, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_client::GetVtxosRequestFilter;
use ark_core::ArkAddress;
use ark_core::script::{csv_sig_script, multisig_script};
use ark_core::send::{OffchainTransactions, SendReceiver, VtxoInput, build_offchain_transactions};
use bitcoin::absolute::LockTime;
use bitcoin::hashes::{Hash, sha256};
use bitcoin::key::Secp256k1;
use bitcoin::opcodes::all::{OP_CLTV, OP_DROP};
use bitcoin::psbt::raw;
use bitcoin::script::Builder;
use bitcoin::{Amount, Network, OutPoint, Psbt, PublicKey, ScriptBuf, TxIn, TxOut, Txid, XOnlyPublicKey, psbt};
use regtest::Regtest;
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::wallet::Outpoint;
use rgb_lib::{ContractId, FileContent, RgbTransfer};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SOLD: u64 = 400;
const PRICE_SAT: u64 = 30_000;
const V_SAT: u64 = 2_000;
const CARRIER_SAT: u64 = 1_000;
const OPRET_SAT: u64 = 1;
const BL_T1: u64 = 9101;
const BL_BUYER: u64 = 9102;
const BL_CHANGE: u64 = 9103;
pub(crate) const EMU: &str = "http://localhost:7073";
const TREE_EXPIRY: u64 = 500;

pub(crate) fn emucov(args: &[&str]) -> anyhow::Result<String> {
    let bin = crate::paths::tools().join("emucov/emucov");
    let o = std::process::Command::new(&bin).args(args).output().map_err(|e| anyhow!("{}: {e} (run scripts/setup-emulator.sh)", bin.display()))?;
    ensure!(o.status.success(), "emucov {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8(o.stdout)?.trim().to_string())
}

pub(crate) fn emulator_up(image: &str, extra: &[&str]) -> anyhow::Result<()> {
    let s = crate::paths::root().join("scripts/emulator-up.sh");
    let ok = std::process::Command::new(s).arg(image).args(extra).status()?.success();
    ensure!(ok, "emulator-up {image} failed");
    Ok(())
}

pub(crate) async fn emu_pubkey() -> anyhow::Result<PublicKey> {
    let v: serde_json::Value = reqwest::get(format!("{EMU}/v1/info")).await?.json().await?;
    let pk = v["signerPubkey"].as_str().ok_or_else(|| anyhow!("emulator info: {v}"))?;
    Ok(PublicKey::from_str(pk)?)
}

/// POST /v1/tx: the emulator runs the covenant, co-signs, submits to arkd and finalizes.
pub(crate) async fn emu_submit(ark: &Psbt, cps: &[Psbt]) -> Result<Psbt, String> {
    let body = serde_json::json!({"arkTx": ark.to_string(), "checkpointTxs": cps.iter().map(|c| c.to_string()).collect::<Vec<_>>()});
    let r = reqwest::Client::new().post(format!("{EMU}/v1/tx")).json(&body).timeout(Duration::from_secs(90)).send().await.map_err(|e| format!("http: {e}"))?;
    let st = r.status();
    let txt = r.text().await.map_err(|e| format!("body: {e}"))?;
    if !st.is_success() {
        return Err(format!("{st}: {}", txt.chars().take(400).collect::<String>()));
    }
    let v: serde_json::Value = serde_json::from_str(&txt).map_err(|e| format!("json: {e}"))?;
    Psbt::from_str(v["signedArkTx"].as_str().unwrap_or_default()).map_err(|e| format!("psbt: {e}"))
}

/// The offer VTXO's taproot.
struct OfferVtxo {
    covenant: String, // hex
    fill: ScriptBuf,
    cancel: ScriptBuf,
    exit: ScriptBuf,
    vtxo: ark_core::Vtxo,
    addr: ArkAddress,
    cancel_height: u32,
}

impl OfferVtxo {
    fn new(info: &ark_core::server::Info, emu: &PublicKey, maker: XOnlyPublicKey, maker_spk: &ScriptBuf, price: u64, cancel_height: u32) -> anyhow::Result<Self> {
        let covenant = emucov(&["script", &maker.to_string(), &hex::encode(maker_spk.as_bytes()), &price.to_string()])?;
        let tweaked = XOnlyPublicKey::from_str(&emucov(&["tweak", &emu.to_string(), &covenant])?)?;
        let server = info.signer_pk.x_only_public_key().0;
        let fill = multisig_script(server, tweaked);
        let mut cancel = Builder::new().push_int(cancel_height as i64).push_opcode(OP_CLTV).push_opcode(OP_DROP).into_script().into_bytes();
        cancel.extend(multisig_script(maker, server).into_bytes());
        let cancel = ScriptBuf::from_bytes(cancel);
        let exit = csv_sig_script(info.unilateral_exit_delay, maker);
        // same tree as arkd (btcd AssembleTaprootScriptTree over the tapscripts list)
        let vtxo = ark_core::Vtxo::new_with_custom_scripts(&Secp256k1::new(), server, maker, vec![fill.clone(), cancel.clone(), exit.clone()], info.unilateral_exit_delay, Network::Regtest)?;
        let addr = vtxo.to_ark_address();
        Ok(Self { covenant, fill, cancel, exit, vtxo, addr, cancel_height })
    }

    fn input(&self, leaf: &ScriptBuf, amount: u64, outpoint: OutPoint) -> anyhow::Result<VtxoInput> {
        let cb = self.vtxo.get_spend_info(leaf.clone())?;
        let lt = if *leaf == self.cancel { Some(LockTime::from_height(self.cancel_height)?) } else { None };
        Ok(VtxoInput::new(
            leaf.clone(),
            lt,
            cb,
            vec![self.fill.clone(), self.cancel.clone(), self.exit.clone()],
            self.addr.to_p2tr_script_pubkey(),
            Amount::from_sat(amount),
            outpoint,
            vec![],
        ))
    }
}

/// Funds an offer VTXO from `from` (Alice's VTXO), returns its outpoint.
async fn fund(alice: &ArkClient, info: &ark_core::server::Info, from: OutPoint, ov: &OfferVtxo, sat: u64) -> anyhow::Result<OutPoint> {
    let a_in = alice.resolve_selected_send_inputs(&[from]).await?.remove(0);
    let (change, change_vtxo) = alice.get_offchain_address().await?;
    let f = build_offchain_transactions(&[SendReceiver::bitcoin(ov.addr.clone(), Amount::from_sat(sat))], &change, &[a_in], info)?;
    let txid = f.ark_tx.unsigned_tx.compute_txid();
    let vout = f.ark_tx.unsigned_tx.output.iter().position(|o| o.script_pubkey == ov.addr.to_p2tr_script_pubkey()).ok_or_else(|| anyhow!("no offer output"))? as u32;
    let p = alice.submit_built_offchain_send(f.ark_tx, f.checkpoint_txs, change_vtxo.owner_pk()).await.map_err(|e| anyhow!("fund submit: {e:?}"))?;
    alice.sign_and_finalize_pending_tx(p).await.map_err(|e| anyhow!("fund finalize: {e:?}"))?;
    Ok(OutPoint { txid, vout })
}

/// [0xde]"prevarktx" on each ark input: the tx that funds the checkpoint input (emulator prevouts).
pub(crate) async fn add_prev_ark_txs(client: &ArkClient, p: &mut Psbt, cps: &[Psbt]) -> anyhow::Result<()> {
    for i in 0..p.inputs.len() {
        let cp_txid = p.unsigned_tx.input[i].previous_output.txid;
        let cp = cps.iter().find(|c| c.unsigned_tx.compute_txid() == cp_txid).ok_or_else(|| anyhow!("no checkpoint for input {i}"))?;
        let prev = cp.unsigned_tx.input[0].previous_output.txid;
        let r = client.network_client().get_virtual_txs(vec![prev.to_string()], None, None).await.map_err(|e| anyhow!("virtual tx {prev}: {e:?}"))?;
        let tx = r.txs.into_iter().find(|t| t.unsigned_tx.compute_txid() == prev).ok_or_else(|| anyhow!("virtual tx {prev} not found"))?;
        p.inputs[i].unknown.insert(raw::Key { type_value: 0xde, key: b"prevarktx".to_vec() }, bitcoin::consensus::serialize(&tx.unsigned_tx));
    }
    Ok(())
}

/// Everything the maker publishes (nothing secret: the signature only authorizes T1/T2 as is).
struct Offer {
    v: OutPoint,
    c1: [u8; 32],
    c2: [u8; 32],
    sig: [u8; 64],
    raw_checkpoint: Vec<u8>,
}

/// What a taker changes in a (bad) fill.
#[derive(Clone, Copy, PartialEq)]
enum Tweak {
    None,
    Underpay,
    WrongC2,
    Redirect,
    WrongC1,
}

struct Fill {
    psbt: Psbt,
    cps: Vec<Psbt>,
    txid: Txid,
    cp_txid: Txid,
}

#[allow(clippy::too_many_arguments)]
async fn build_fill(bob: &ArkClient, info: &ark_core::server::Info, ov: &OfferVtxo, offer: &Offer, b: OutPoint, bob_addr: &ArkAddress, maker_spk: &ScriptBuf, tw: Tweak) -> anyhow::Result<Fill> {
    let b_in = bob.resolve_selected_send_inputs(&[b]).await?.remove(0);
    let b_sat = b_in.amount().to_sat();
    let c1 = if tw == Tweak::WrongC1 { [0x11u8; 32] } else { offer.c1 };
    let v_in = ov.input(&ov.fill, V_SAT, offer.v)?.with_checkpoint_op_return(Some(c1));
    let own: OffchainTransactions = build_offchain_transactions(&[SendReceiver::bitcoin(bob_addr.clone(), b_in.amount())], bob_addr, &[b_in, v_in], info)?;
    let vin = own.ark_tx.unsigned_tx.input.iter().position(|i| own.checkpoint_txs.iter().any(|c| c.unsigned_tx.compute_txid() == i.previous_output.txid && c.unsigned_tx.input[0].previous_output == offer.v)).ok_or_else(|| anyhow!("no V input"))?;
    let bin = 1 - vin;
    let cp_v = own.checkpoint_txs.iter().find(|c| c.unsigned_tx.input[0].previous_output == offer.v).unwrap().clone();
    let raw_cp = bitcoin::consensus::serialize(&cp_v.unsigned_tx);
    ensure!(tw == Tweak::WrongC1 || raw_cp == offer.raw_checkpoint, "checkpoint of V differs from the offer's");
    let bob_spk = bob_addr.to_p2tr_script_pubkey();
    let anchor = own.ark_tx.unsigned_tx.output.iter().find(|o| o.value == Amount::ZERO && !o.script_pubkey.is_op_return()).cloned().ok_or_else(|| anyhow!("no anchor"))?;
    let mut pay = TxOut { value: Amount::from_sat(PRICE_SAT), script_pubkey: maker_spk.clone() };
    let mut change = b_sat + V_SAT - PRICE_SAT - CARRIER_SAT - OPRET_SAT;
    let mut c2 = offer.c2;
    match tw {
        Tweak::Underpay => {
            pay.value = Amount::from_sat(PRICE_SAT - 1);
            change += 1;
        }
        Tweak::Redirect => pay.script_pubkey = bob_spk.clone(),
        Tweak::WrongC2 => c2 = [0x22u8; 32],
        _ => {}
    }
    let mut opret = bare_opret(Amount::from_sat(OPRET_SAT));
    opret.script_pubkey = ScriptBuf::new_op_return(c2);
    // the taker always presents the maker's published witness (C1, C2, sig) and the spent checkpoint
    let w = format!("{},{},{},{}", hex::encode(offer.sig), hex::encode(offer.c1), hex::encode(offer.c2), hex::encode(&raw_cp));
    let ext = ScriptBuf::from_bytes(hex::decode(emucov(&["ext", &vin.to_string(), &ov.covenant, &w])?)?);
    let mut tx = own.ark_tx.unsigned_tx.clone();
    tx.output = vec![
        opret,
        pay,
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: bob_spk.clone() },
        TxOut { value: Amount::from_sat(change), script_pubkey: bob_spk.clone() },
        TxOut { value: Amount::ZERO, script_pubkey: ext },
        anchor,
    ];
    let mut p = Psbt::from_unsigned_tx(tx)?;
    p.inputs = own.ark_tx.inputs.clone();
    for i in p.inputs.iter_mut() {
        i.sighash_type = None;
    }
    add_prev_ark_txs(bob, &mut p, &own.checkpoint_txs).await?;
    bob.sign_ark_tx_inputs(&mut p, &[bin]).map_err(|e| anyhow!("{e:?}"))?;
    let mut cps = own.checkpoint_txs.clone();
    for c in cps.iter_mut() {
        if c.unsigned_tx.input[0].previous_output == b {
            if c.inputs[0].witness_script.is_none() {
                c.inputs[0].witness_script = own.ark_tx.inputs[bin].witness_script.clone();
            }
            bob.sign_checkpoint_tx(c).map_err(|e| anyhow!("{e:?}"))?; // the emulator finalizes: taker presigns
        }
    }
    let txid = p.unsigned_tx.compute_txid();
    Ok(Fill { psbt: p, cps, txid, cp_txid: cp_v.unsigned_tx.compute_txid() })
}

/// Last "failed to process" line of the emulator (the HTTP error only says "internal error").
pub(crate) fn emu_last_error() -> String {
    let Ok(o) = std::process::Command::new("docker").args(["logs", "--tail", "50", "emulator"]).output() else { return String::new() };
    let all = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    all.lines().filter(|l| l.contains("level=error")).last().map(|l| l.split("error=").nth(1).unwrap_or(l).chars().take(300).collect()).unwrap_or_default()
}

pub(crate) async fn spent(v: OutPoint) -> String {
    match crate::long_offline::vtxo_info(v).await {
        Some(x) => format!("spent {} swept {} preconfirmed {} spent_by {:?}", x.is_spent, x.is_swept, x.is_preconfirmed, x.spent_by),
        None => "no info".into(),
    }
}

pub async fn run(mode: Option<String>) -> anyhow::Result<()> {
    let crash = mode.as_deref() == Some("crash");
    let run_id = format!("run-{}-swap-emu{}", now(), if crash { "-crash" } else { "" });
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

    step("0. emulator:rgb next to arkd cponly");
    emulator_up("emulator:rgb", &[])?;
    let emu = emu_pubkey().await?;
    rec("emulator_pubkey", emu.to_string());

    step("1. Alice (maker, single-key client), Bob (taker), Alice's spare C board");
    let (alice, akp) = keypair_client("alice", regtest.clone(), &state).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    let (alice_c, _) = ark_client("alice-c", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    rec("arkd_version", info.version.clone());
    board("alice", &alice, "0.001").await?;
    board("bob", &bob, "0.001").await?;
    board("alice-c", &alice_c, "0.0002").await?;
    let h1 = height(&regtest)?;
    let (a0, b, c) = (first(&alice, "alice").await?, first(&bob, "bob").await?, first(&alice_c, "alice-c").await?);
    rec("vtxos", format!("Alice A {a0}, Alice spare C {c}, Bob B {b} (batch 1 at height {h1})"));
    let (alice_addr, _) = alice.get_offchain_address().await?;
    let maker_spk = alice_addr.to_p2tr_script_pubkey();
    let maker = akp.x_only_public_key().0;
    let (bob_addr, _) = bob.get_offchain_address().await?;

    step("2. Alice creates the offer VTXO V (fill / cancel / exit leaves) and funds it offchain");
    let t0 = Instant::now();
    let cancel_h = (height(&regtest)? + 20) as u32;
    let ov = OfferVtxo::new(&info, &emu, maker, &maker_spk, PRICE_SAT, cancel_h)?;
    rec("covenant", format!("{} bytes: {}", ov.covenant.len() / 2, ov.covenant));
    rec("leaves", format!("fill {} B, cancel {} B (CLTV {cancel_h}), exit {} B", ov.fill.len(), ov.cancel.len(), ov.exit.len()));
    rec("offer_address", ov.addr.to_string());
    let v = fund(&alice, &info, a0, &ov, V_SAT).await?;
    rec("v", format!("{v} ({V_SAT} sat): {}", spent(v).await));
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;
    let (mut carol_rgb, carol_online) = bl(|| rgb_wallet("carol", &state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, v, &state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    rec("asset_on_v", format!("{asset_id} on {v}"));

    step("3. Alice computes T1 (C1) and T2 (C2), signs sha256(C1||C2||V), publishes, goes offline");
    let t_offer = Instant::now();
    let v_in = ov.input(&ov.fill, V_SAT, v)?;
    let recv = vec![SendReceiver::bitcoin(alice_addr.clone(), Amount::from_sat(V_SAT))];
    let dry = build_offchain_transactions(&recv, &alice_addr, &[v_in.clone().with_checkpoint_op_return(Some([0u8; 32]))], &info)?;
    let mut cp = dry.checkpoint_txs[0].clone();
    cp.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    bl(|| alice_rgb.color_psbt_and_consume_custom(&mut cp, cid, vec![(SealTarget::WitnessVout(1), SOLD, BL_T1), (SealTarget::Explicit(Outpoint { txid: c.txid.to_string(), vout: c.vout }), ISSUE_AMOUNT - SOLD, BL_CHANGE)]))?;
    let mut c1 = [0u8; 32];
    c1.copy_from_slice(&cp.unsigned_tx.output[0].script_pubkey.as_bytes()[2..]);
    let built = build_offchain_transactions(&recv, &alice_addr, &[v_in.with_checkpoint_op_return(Some(c1))], &info)?;
    let checkpoint = built.checkpoint_txs[0].clone();
    let cp_txid = checkpoint.unsigned_tx.compute_txid();
    ensure!(cp_txid == cp.unsigned_tx.compute_txid(), "checkpoint changed between the two builds");
    // T2 is colored on a template with the final shape: [opret, maker, carrier(vout 2)], CP:1 at input 1
    let mut tx = built.ark_tx.unsigned_tx.clone();
    tx.input = vec![TxIn::default(), built.ark_tx.unsigned_tx.input[0].clone()];
    tx.output = vec![
        bare_opret(Amount::from_sat(OPRET_SAT)),
        TxOut { value: Amount::from_sat(PRICE_SAT), script_pubkey: maker_spk.clone() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: maker_spk.clone() },
    ];
    let mut template = Psbt::from_unsigned_tx(tx)?;
    template.inputs = vec![psbt::Input::default(), built.ark_tx.inputs[0].clone()];
    let t2: RgbTransfer = bl(|| alice_rgb.color_psbt_and_consume_custom(&mut template, cid, vec![(SealTarget::WitnessVout(2), SOLD, BL_BUYER)]))?;
    let mut c2 = [0u8; 32];
    c2.copy_from_slice(&template.unsigned_tx.output[0].script_pubkey.as_bytes()[2..]);
    let template_txid = template.unsigned_tx.compute_txid();
    // bound to V's outpoint too (the covenant reads it from the checkpoint's input 0)
    let msg = sha256::Hash::hash(&[&c1[..], &c2[..], &bitcoin::consensus::serialize(&v)[..]].concat());
    let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&bitcoin::secp256k1::Message::from_digest(msg.to_byte_array()), &akp);
    let offer = Offer { v, c1, c2, sig: sig.serialize(), raw_checkpoint: bitcoin::consensus::serialize(&checkpoint.unsigned_tx) };
    let consignment = state.join("offer.rgbc");
    t2.save_file(&consignment).map_err(|e| anyhow!("save: {e}"))?;
    let offer_proof = proof_path(&consignment);
    build_proof(&alice, &regtest, &[], &[], &offer_proof).await?;
    add_vtxos(&alice, &regtest, &offer_proof, &[v]).await?;
    // the published offer: protobuf (proto/offers.proto); the JSON of the same content is only measured
    let offer_pb = crate::wire::SwapEmuOffer {
        v: Some(crate::wire::outpoint(&v)),
        covenant: hex::decode(&ov.covenant)?,
        cancel_height: cancel_h,
        maker_xonly: maker.serialize().to_vec(),
        emulator_pubkey: emu.to_bytes(),
        c1: c1.to_vec(),
        c2: c2.to_vec(),
        maker_sig: offer.sig.to_vec(),
        template_psbt: template.serialize(),
        sell_units: SOLD,
        price_sat: PRICE_SAT,
        asset: asset_id.clone(),
    };
    let offer_bytes = prost::Message::encode_to_vec(&offer_pb);
    std::fs::write(state.join("offer.pb"), &offer_bytes)?;
    // the taker reads the published file
    let back: crate::wire::SwapEmuOffer = prost::Message::decode(std::fs::read(state.join("offer.pb"))?.as_slice())?;
    ensure!(back == offer_pb && back.c1 == c1 && back.c2 == c2 && back.maker_sig == offer.sig && Psbt::deserialize(&back.template_psbt)? == template, "offer.pb roundtrip");
    let legacy_json = serde_json::to_vec(&serde_json::json!({
        "v": v.to_string(), "covenant": ov.covenant, "cancel_height": cancel_h,
        "maker_xonly": maker.to_string(), "emulator_pubkey": emu.to_string(),
        "c1": hex::encode(c1), "c2": hex::encode(c2), "maker_sig": hex::encode(offer.sig),
        "template_psbt": template.to_string(),
        "sell_units": SOLD, "price_sat": PRICE_SAT, "asset": asset_id,
    }))?;
    rec("offer_build_ms", t_offer.elapsed().as_millis().to_string());
    rec("setup_to_offer_ms", t0.elapsed().as_millis().to_string());
    rec("offer_pb_bytes", format!("{} (gzip {})", offer_bytes.len(), crate::wire::gz_len(&offer_bytes)));
    rec("offer_json_bytes", format!("{} (gzip {}) [same content as JSON, not written]", legacy_json.len(), crate::wire::gz_len(&legacy_json)));
    rec("offer_consignment_bytes", file_len(&consignment).to_string());
    rec("offer_arkproof_bytes", file_len(&offer_proof).to_string());
    rec("offer_arkproof_bytes_cmp", crate::proof::size_cmp(&offer_proof)?);
    rec("checkpoint_txid", cp_txid.to_string());
    println!("[alice] offer published; Alice is offline from here");

    step("4. Bob checks the offer offline (template witness)");
    let pre = bl(|| bob_rgb.validate_consignment_offchain(bob_online.clone(), consignment.clone(), vec![cp_txid.to_string(), template_txid.to_string()], template_txid.to_string(), 2))?;
    rec("bob_offer_check_units_vout2", units(&pre).to_string());
    ensure!(units(&pre) == SOLD, "offer gives {} on vout 2", units(&pre));

    if crash {
        never_submitted(&mut rec, &regtest, &alice, &akp, &alice_addr, &bob, &info, &emu, b, &bob_addr, &maker_spk).await?;
    } else {
        step("5. negatives against the emulator (each must be refused; V and B stay unspent)");
        for (name, tw) in [("underpay", Tweak::Underpay), ("wrong_c2_commitment", Tweak::WrongC2), ("wrong_c1_checkpoint", Tweak::WrongC1), ("redirect_maker_output", Tweak::Redirect)] {
            let f = build_fill(&bob, &info, &ov, &offer, b, &bob_addr, &maker_spk, tw).await?;
            let r = emu_submit(&f.psbt, &f.cps).await;
            rec(&format!("neg_{name}"), format!("{:?} / emulator log: {}", r.as_ref().err(), emu_last_error()));
            ensure!(r.is_err(), "emulator accepted {name}");
    }
    rec("after_negatives", format!("V {}; B {}", spent(v).await, spent(b).await));
    }

    step("6. Bob fills via the emulator");
    let t_fill = Instant::now();
    let f = build_fill(&bob, &info, &ov, &offer, b, &bob_addr, &maker_spk, Tweak::None).await?;
    ensure!(f.cp_txid == cp_txid, "fill checkpoint != offer checkpoint");
    let final_txid = f.txid;
    rec("final_txid", final_txid.to_string());
    rec("final_outputs", format!("{:?}", outs(&f.psbt)));
    rec("final_ark_tx_vbytes", f.psbt.unsigned_tx.vsize().to_string());
    rec("extension_output_bytes", f.psbt.unsigned_tx.output[4].script_pubkey.len().to_string());
    let fin_cons = state.join("filled.rgbc");
    let n = reanchor(&consignment, &fin_cons, template_txid, &f.psbt.unsigned_tx)?;
    rec("reanchored_bundles", n.to_string());
    let got = bl(|| bob_rgb.validate_consignment_offchain(bob_online.clone(), fin_cons.clone(), vec![cp_txid.to_string(), final_txid.to_string()], final_txid.to_string(), 2))?;
    rec("bob_check_final_vout2", units(&got).to_string());
    ensure!(units(&got) == SOLD, "re-anchored consignment gives {}", units(&got));
    let bob_v = OutPoint { txid: final_txid, vout: 2 };
    if crash {
        crash_after_submit(&mut rec, &bob, &f, v, b, bob_v).await?;
    } else {
        let t_emu = Instant::now();
        let signed = emu_submit(&f.psbt, &f.cps).await.map_err(|e| anyhow!("emulator: {e} / {}", emu_last_error()))?;
        rec("emulator_submit_ms", t_emu.elapsed().as_millis().to_string());
        ensure!(signed.unsigned_tx.compute_txid() == final_txid, "emulator returned another tx");
    }
    let alice_v = OutPoint { txid: final_txid, vout: 1 };
    wait_vtxo(&bob, bob_v, "bob").await?;
    rec("fill_ms", t_fill.elapsed().as_millis().to_string());
    wait_vtxo(&alice, alice_v, "alice").await?;
    let alice_sat = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().find(|x| x.vtxo().outpoint == alice_v).map(|x| x.vtxo().amount.to_sat()).unwrap_or(0);
    rec("alice_paid_vtxo_sat", alice_sat.to_string());
    ensure!(alice_sat == PRICE_SAT, "Alice got {alice_sat}");
    rec("v_after_fill", spent(v).await);
    let emu_logs = std::process::Command::new("docker").args(["logs", "emulator"]).output()?;
    let el = format!("{}{}", String::from_utf8_lossy(&emu_logs.stdout), String::from_utf8_lossy(&emu_logs.stderr));
    rec("emulator_log_finalize", el.lines().filter(|l| l.contains("finaliz")).map(|l| l.chars().take(200).collect::<String>()).collect::<Vec<_>>().join(" | "));

    step("7. Bob validates with arkd paused (consignment + Ark proof) and accepts 400 on final:2");
    let pp = proof_path(&fin_cons);
    let st = build_proof(&bob, &regtest, &[cp_txid, final_txid], &[], &pp).await?;
    add_vtxos(&bob, &regtest, &pp, &[bob_v, alice_v, c]).await?;
    rec("fill_arkproof", format!("{} bytes, {} psbts", st.bytes, st.n_psbts));
    rec("fill_arkproof_cmp", crate::proof::size_cmp(&pp)?);
    rec("fill_consignment_bytes", file_len(&fin_cons).to_string());
    let ops = operators(&info);
    {
        let _down = arkd_down(&mut rec, "bob")?;
        import(&bob_rgb, &bob_online, &pp, &ops)?;
        let (_, a) = bl(|| bob_rgb.accept_transfer_consignment_offchain(bob_online.clone(), fin_cons.clone(), vec![], final_txid.to_string(), 2, BL_BUYER))?;
        rec("bob_accepted", units(&a).to_string());
        ensure!(units(&a) == SOLD, "Bob accepted {}", units(&a));
    }

    step("8. Alice back online: 600 change on C validates and is registered");
    import(&alice_rgb, &alice_online, &pp, &ops)?;
    let ch = bl(|| alice_rgb.validate_consignment_offchain(alice_online.clone(), fin_cons.clone(), vec![], c.txid.to_string(), c.vout))?;
    rec("alice_change_on_c", units(&ch).to_string());
    ensure!(units(&ch) == ISSUE_AMOUNT - SOLD, "Alice change {}", units(&ch));
    let reg_c = bl(|| alice_rgb.ark_receive_vtxo_transfer(alice_online.clone(), fin_cons.clone(), c.to_string())).map_err(|e| anyhow!("{e}"));
    rec("alice_registers_change_on_c", format!("{:?}", reg_c.as_ref().map_err(|e| e.to_string())));
    if crash {
        let again = emu_submit(&f.psbt, &f.cps).await;
        rec("crash_resubmit_after_recovery", format!("{:?} / {}", again.as_ref().map(|p| p.unsigned_tx.compute_txid().to_string()), emu_last_error()));
        rec("crash_pending_records_end", emu_pending());
        println!("SWAP-EMU-CRASH RESULT: PASS");
        return Ok(());
    }

    step("9. cancel path on a second (bitcoin-only) offer V2; and a direct arkd submit of a fill without the emulator");
    let a1 = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().map(|x| x.vtxo().outpoint).find(|o| *o != alice_v).ok_or_else(|| anyhow!("alice has no change VTXO"))?;
    let cancel_h2 = (height(&regtest)? + 10) as u32;
    let ov2 = OfferVtxo::new(&info, &emu, maker, &maker_spk, PRICE_SAT, cancel_h2)?;
    let v2 = fund(&alice, &info, a1, &ov2, 3_000).await?;
    rec("v2", format!("{v2} (cancel CLTV {cancel_h2}, now {})", height(&regtest)?));
    // fill leaf without the emulator: Bob sends V2's fill-leaf spend straight to arkd
    let v2_fill = ov2.input(&ov2.fill, 3_000, v2)?;
    let direct = build_offchain_transactions(&[SendReceiver::bitcoin(bob_addr.clone(), Amount::from_sat(3_000))], &bob_addr, &[v2_fill], &info)?;
    let r = bob.network_client().submit_offchain_transaction_request(direct.ark_tx, direct.checkpoint_txs).await;
    rec("neg_direct_arkd_without_emulator", format!("{:?}", r.as_ref().err().map(|e| format!("{e:?}").chars().take(300).collect::<String>())));
    ensure!(r.is_err(), "arkd accepted the fill leaf without the emulator signature");
    let cancel = |h: u32| {
        let ov2 = &ov2;
        let alice = &alice;
        let info = &info;
        let alice_addr = alice_addr.clone();
        async move {
            let _ = h;
            let i = ov2.input(&ov2.cancel, 3_000, v2)?;
            let t = build_offchain_transactions(&[SendReceiver::bitcoin(alice_addr.clone(), Amount::from_sat(3_000))], &alice_addr, &[i], info)?;
            let txid = t.ark_tx.unsigned_tx.compute_txid();
            let p = alice.submit_built_offchain_send(t.ark_tx, t.checkpoint_txs, maker).await.map_err(|e| anyhow!("{}", format!("{e:?}").chars().take(300).collect::<String>()))?;
            alice.sign_and_finalize_pending_tx(p).await.map_err(|e| anyhow!("finalize: {e:?}"))?;
            anyhow::Ok(txid)
        }
    };
    let early = cancel(cancel_h2).await;
    rec("cancel_before_cltv", format!("{:?}", early.as_ref().map_err(|e| e.to_string())));
    ensure!(early.is_err(), "cancel accepted before the CLTV height");
    mine_to(&regtest, cancel_h2 as u64)?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let ok = cancel(cancel_h2).await.map_err(|e| anyhow!("cancel after CLTV: {e}"))?;
    wait_vtxo(&alice, OutPoint { txid: ok, vout: 0 }, "alice").await?;
    rec("cancel_after_cltv", format!("ark tx {ok}; V2 {}", spent(v2).await));

    step("10. a later batch: fresh VTXOs; renewals of the received outputs");
    regtest::mine(240)?;
    let (alice_g, _) = ark_client("alice-g", regtest.clone(), &state).await?;
    let (bob_g, _) = ark_client("bob-g", regtest.clone(), &state).await?;
    let (carol, _) = ark_client("carol", regtest.clone(), &state).await?;
    board("alice-g", &alice_g, "0.0005").await?;
    board("bob-g", &bob_g, "0.0005").await?;
    board("carol", &carol, "0.0002").await?;
    let h2 = height(&regtest)?;
    let (ga, gb, rv) = (first(&alice_g, "alice-g").await?, first(&bob_g, "bob-g").await?, first(&carol, "carol").await?);
    rec("batch2", format!("height {h2}: Ga {ga}, Gb {gb}, Carol R {rv}"));
    let (cp_rb, _, ark_rb) = checkpoint_hop(&bob, &bob_rgb, &info, bob_v, cid, SOLD, gb, 9121).await?;
    rec("renew_bob", format!("checkpoint {cp_rb}, ark tx {ark_rb}"));
    let (cp_ra, _, ark_ra) = checkpoint_hop(&alice_c, &alice_rgb, &info, c, cid, ISSUE_AMOUNT - SOLD, ga, 9122).await?;
    rec("renew_alice_change", format!("checkpoint {cp_ra}, ark tx {ark_ra}"));
    let st = alice.settle_all(&mut rand::thread_rng()).await;
    rec("alice_btc_settle", format!("{:?}", st.as_ref().map_err(|e| format!("{e:?}").chars().take(300).collect::<String>())));

    step("11. batch 1 expires and is swept; Bob sends the renewed 400 Gb -> Carol, validated offline");
    mine_to(&regtest, h1 + TREE_EXPIRY + 20)?;
    let left = OutPoint { txid: ark_rb, vout: 0 };
    let mut swept = false;
    for _ in 0..40 {
        if crate::long_offline::vtxo_info(left).await.is_some_and(|x| x.is_swept) {
            swept = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    rec("batch1_swept", format!("height {}: {left} swept {swept}; V {}", height(&regtest)?, spent(v).await));
    let (cp_bc, to_carol_t, ark_bc) = checkpoint_hop(&bob_g, &bob_rgb, &info, gb, cid, SOLD, rv, 9131).await?;
    rec("bob_to_carol", format!("checkpoint {cp_bc}, ark tx {ark_bc}"));
    let to_carol = state.join("to-carol.rgbc");
    to_carol_t.save_file(&to_carol).map_err(|e| anyhow!("save: {e}"))?;
    let pc = proof_path(&to_carol);
    let stc = build_proof(&bob_g, &regtest, &[cp_txid, final_txid, cp_rb, cp_bc], &[], &pc).await?;
    add_vtxos(&carol, &regtest, &pc, &[rv]).await?;
    rec("carol_proof", format!("{} bytes, {} psbts; consignment {} bytes", stc.bytes, stc.n_psbts, file_len(&to_carol)));
    rec("carol_proof_cmp", crate::proof::size_cmp(&pc)?);
    {
        let _down = arkd_down(&mut rec, "carol")?;
        import(&carol_rgb, &carol_online, &pc, &ops)?;
        let got = bl(|| carol_rgb.ark_receive_vtxo_transfer(carol_online.clone(), to_carol.clone(), rv.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("carol_received", got.to_string());
        ensure!(got == SOLD, "Carol received {got}");
    }
    println!("SWAP-EMU RESULT: PASS");
    Ok(())
}

/// Pending finalize records in the emulator's datadir (volume emulator-data), via docker cp.
pub(crate) fn emu_pending() -> String {
    let d = std::env::temp_dir().join(format!("emu-pending-{}", now()));
    let ok = std::process::Command::new("docker").args(["cp", "emulator:/app/data/pending", d.to_str().unwrap()]).output().is_ok_and(|o| o.status.success());
    if !ok {
        return "no pending dir".into();
    }
    let names: Vec<String> = std::fs::read_dir(&d).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&d);
    format!("{} record(s) {names:?}", names.len())
}

pub(crate) fn emu_state() -> String {
    std::process::Command::new("docker").args(["inspect", "emulator", "-f", "{{.State.Status}} exit={{.State.ExitCode}}"]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// N: the emulator (test image) crashes after persisting the record but BEFORE arkd SubmitTx, on a
/// bitcoin-only offer V2. On restart the record is dropped (arkd holds nothing pending), V2 stays
/// free and the maker cancels after the CLTV.
#[allow(clippy::too_many_arguments)]
async fn never_submitted(
    rec: &mut dyn FnMut(&str, String),
    regtest: &Regtest,
    alice: &ArkClient,
    akp: &bitcoin::key::Keypair,
    alice_addr: &ArkAddress,
    bob: &ArkClient,
    info: &ark_core::server::Info,
    emu: &PublicKey,
    b: OutPoint,
    bob_addr: &ArkAddress,
    maker_spk: &ScriptBuf,
) -> anyhow::Result<()> {
    step("N1. bitcoin-only offer V2 (same covenant shape, fixed C1/C2)");
    let maker = akp.x_only_public_key().0;
    let a1 = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().map(|x| x.vtxo().outpoint).next().ok_or_else(|| anyhow!("alice has no spendable VTXO"))?;
    let cancel_h2 = (height(regtest)? + 10) as u32;
    let ov2 = OfferVtxo::new(info, emu, maker, maker_spk, PRICE_SAT, cancel_h2)?;
    let v2 = fund(alice, info, a1, &ov2, V_SAT).await?;
    let (c1, c2) = ([0x5au8; 32], [0x5bu8; 32]);
    let recv = vec![SendReceiver::bitcoin(alice_addr.clone(), Amount::from_sat(V_SAT))];
    let built = build_offchain_transactions(&recv, alice_addr, &[ov2.input(&ov2.fill, V_SAT, v2)?.with_checkpoint_op_return(Some(c1))], info)?;
    let msg = sha256::Hash::hash(&[&c1[..], &c2[..], &bitcoin::consensus::serialize(&v2)[..]].concat());
    let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&bitcoin::secp256k1::Message::from_digest(msg.to_byte_array()), akp);
    let offer2 = Offer { v: v2, c1, c2, sig: sig.serialize(), raw_checkpoint: bitcoin::consensus::serialize(&built.checkpoint_txs[0].unsigned_tx) };
    rec("n_v2", format!("{v2} (cancel CLTV {cancel_h2}, now {})", height(regtest)?));

    step("N2. emulator:rgb-crashtest EMULATOR_RGB_TEST_CRASH=before; Bob fills V2");
    emulator_up("emulator:rgb-crashtest", &["-e", "EMULATOR_RGB_TEST_CRASH=before"])?;
    let f = build_fill(bob, info, &ov2, &offer2, b, bob_addr, maker_spk, Tweak::None).await?;
    let r = emu_submit(&f.psbt, &f.cps).await;
    rec("n_submit", format!("{:?}", r.as_ref().map(|p| p.unsigned_tx.compute_txid()).err()));
    ensure!(r.is_err(), "crash-before image answered");
    tokio::time::sleep(Duration::from_secs(3)).await;
    rec("n_emulator_state", emu_state());
    rec("n_pending_records_after_crash", emu_pending());
    rec("n_after_crash", format!("V2 {}; B {}", spent(v2).await, spent(b).await));

    step("N3. emulator:rgb restarted on the same datadir: the record is dropped");
    emulator_up("emulator:rgb", &[])?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let pend = emu_pending();
    rec("n_pending_records_after_restart", pend.clone());
    ensure!(pend.starts_with("0 record"), "record not dropped: {pend}");
    rec("n_after_restart", format!("V2 {}; B {}", spent(v2).await, spent(b).await));

    step("N4. maker cancels V2 after its CLTV");
    mine_to(regtest, cancel_h2 as u64)?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let i = ov2.input(&ov2.cancel, V_SAT, v2)?;
    let t = build_offchain_transactions(&[SendReceiver::bitcoin(alice_addr.clone(), Amount::from_sat(V_SAT))], alice_addr, &[i], info)?;
    let txid = t.ark_tx.unsigned_tx.compute_txid();
    let p = alice.submit_built_offchain_send(t.ark_tx, t.checkpoint_txs, maker).await.map_err(|e| anyhow!("cancel: {}", format!("{e:?}").chars().take(300).collect::<String>()))?;
    alice.sign_and_finalize_pending_tx(p).await.map_err(|e| anyhow!("cancel finalize: {e:?}"))?;
    wait_vtxo(alice, OutPoint { txid, vout: 0 }, "alice").await?;
    rec("n_cancel_after_cltv", format!("ark tx {txid}; V2 {}", spent(v2).await));
    Ok(())
}

/// C: the emulator (test image) crashes right AFTER arkd accepted the fill (arkd holds V and B
/// pending). The normal image restarted on the same datadir finds the record, gets arkd's
/// checkpoint signatures with GetPendingTx and finalizes, with no action from Bob or Alice.
async fn crash_after_submit(rec: &mut dyn FnMut(&str, String), bob: &ArkClient, f: &Fill, v: OutPoint, b: OutPoint, bob_v: OutPoint) -> anyhow::Result<()> {
    step("C1. emulator:rgb-crashtest EMULATOR_RGB_TEST_CRASH=after; Bob fills V");
    emulator_up("emulator:rgb-crashtest", &["-e", "EMULATOR_RGB_TEST_CRASH=after"])?;
    let r = emu_submit(&f.psbt, &f.cps).await;
    rec("crash_submit", format!("{:?}", r.as_ref().map(|p| p.unsigned_tx.compute_txid()).err()));
    ensure!(r.is_err(), "crash-after image answered");
    tokio::time::sleep(Duration::from_secs(3)).await;
    rec("crash_emulator_state", emu_state());
    rec("crash_pending_records_after_crash", emu_pending());
    rec("crash_after", format!("V {}; B {}; final:2 {}", spent(v).await, spent(b).await, spent(bob_v).await));

    step("C2. emulator:rgb restarted on the same datadir; nobody resubmits");
    let t = Instant::now();
    emulator_up("emulator:rgb", &[])?;
    let done = tokio::time::timeout(Duration::from_secs(90), wait_vtxo(bob, bob_v, "bob")).await;
    rec("crash_recovered_ms", format!("{} ms after restart: {:?}", t.elapsed().as_millis(), done.as_ref().map(|r| r.is_ok())));
    ensure!(done.as_ref().is_ok_and(|r| r.is_ok()), "fill not finalized after restart");
    let el = std::process::Command::new("docker").args(["logs", "emulator"]).output()?;
    let el = format!("{}{}", String::from_utf8_lossy(&el.stdout), String::from_utf8_lossy(&el.stderr));
    rec("crash_emulator_log", el.lines().filter(|l| l.contains("pending")).map(|l| l.chars().take(200).collect::<String>()).collect::<Vec<_>>().join(" | "));
    rec("crash_pending_records_after_recovery", emu_pending());
    Ok(())
}

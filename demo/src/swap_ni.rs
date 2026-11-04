//! Scenario `swap-ni`: NON-interactive sale of an RGB asset for bitcoin within Arkade, on arkd with
//! only the checkpoint data OP_RETURN (profile `cponly`, arkd master f863e48 + checkpoint-opret).
//!
//! The seller (maker) publishes an offer and goes offline; any buyer (taker) can fill it alone:
//!   checkpoint(V): OP_RETURN T1 (V -> SOLD units on witness vout 1 = CP:1, change on the maker's
//!     existing spare VTXO C), presigned by the maker (DEFAULT): T1 is fixed by that signature
//!   ark tx input 0 (CP:1): maker's signature SIGHASH_SINGLE|ANYONECANPAY, which commits only to
//!     its own input and output 0 = maker's address, value V + price (and the asset change)
//!   T2 (CP:1 -> witness vout 2: SOLD units; the change must NOT ride in T2, whose opret the
//!     maker's SINGLE signature does not cover, so a taker could rewrite it), committed by a
//!     32-byte opret the maker computes in advance: an opret commitment and witness-vout seals do
//!     not depend on the txid, so the taker copies it into the first OP_RETURN (vout 1, 1 sat:
//!     stock arkd's sub-dust rule for OP_RETURN <32B>)
//!   the taker adds its own input (paying), its asset carrier at vout 2 and its change, signs,
//!     submits, re-attaches the maker's checkpoint signature and finalizes
//! The consignment's T2 witness is the maker's template; the taker re-anchors it to the final tx
//! (the transitions and the commitment stay byte-identical, only the bundled witness tx changes).
use crate::checkpoint_offline::unroll;
use crate::checkpoint_renew::checkpoint_hop;
use crate::exit_watchtower::Watchtower;
use crate::forfeit::spend_exited;
use crate::offchain_transfer::{outs, wait_vtxo};
use crate::proof::{add_vtxos, arkd_down, build_proof, file_len, import, operators, proof_path};
use crate::{ArkClient, ISSUE_AMOUNT, ark_client, bare_opret, bl, board, now, onchain_genesis_jump_in, regtest, rgb_wallet, step};
use anyhow::{anyhow, ensure};
use ark_bdk_wallet::Wallet as ArkBdkWallet;
use ark_client::{GetVtxosRequestFilter, InMemorySwapStorage, OfflineClient, OfflineClientConfig};
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions, sign_checkpoint_transaction};
use bitcoin::hashes::Hash;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::psbt::PsbtSighashType;
use bitcoin::secp256k1::{self, SecretKey};
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::taproot::{self, TapLeafHash};
use bitcoin::{Amount, Network, OutPoint, Psbt, ScriptBuf, TapSighashType, Transaction, TxIn, TxOut, Txid, psbt};
use regtest::Regtest;
use rgb_lib::wallet::{Outpoint, RgbWalletOpsOnline};
use rgb_lib::wallet::rust_only::SealTarget;
use rgb_lib::{Assignment, ContractId, FileContent, PubWitness, RgbTransfer};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

/// index of the maker's SINGLE|ACP input and of the output it pays
const MK: usize = 1;
const SOLD: u64 = 400;
const PRICE_SAT: u64 = 30_000;
const CARRIER_SAT: u64 = 1_000;
const OPRET_SAT: u64 = 1;
const BL_T1: u64 = 9001;
const BL_BUYER: u64 = 9002;
const BL_CHANGE: u64 = 9003;

/// A single-key Ark client (the maker signs with a custom sighash, so it needs its key).
pub(crate) async fn keypair_client(name: &str, rt: Arc<Regtest>, state: &Path) -> anyhow::Result<(ArkClient, Keypair)> {
    let secp = Secp256k1::new();
    let kp = Keypair::from_secret_key(&secp, &SecretKey::new(&mut rand::thread_rng()));
    let mut sk2 = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut sk2);
    std::fs::write(state.join(format!("{name}-ark-sk.hex")), hex::encode(kp.secret_bytes()))?;
    let wkp = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&sk2)?);
    let wallet = Arc::new(ArkBdkWallet::new(wkp, Network::Regtest, "http://localhost:3000/api")?);
    let c = OfflineClient::with_keypair(
        OfflineClientConfig { ark_server_url: "http://localhost:7070".into(), boltz_url: "http://localhost:9069".into(), ..Default::default() },
        kp,
        rt,
        wallet,
        Arc::new(InMemorySwapStorage::default()),
    )
    .connect_with_retries(5)
    .await
    .map_err(|e| anyhow!("{name}: connect: {e:?}"))?;
    Ok((c, kp))
}

fn signer(kp: Keypair) -> impl FnOnce(&mut psbt::Input, secp256k1::Message) -> Result<Vec<(secp256k1::schnorr::Signature, bitcoin::XOnlyPublicKey)>, ark_core::Error> {
    move |_, msg| Ok(vec![(Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp), kp.x_only_public_key().0)])
}

/// Sign input `i` with SIGHASH_SINGLE|ANYONECANPAY: commits to its own prevout and output `i` only.
fn sign_single_acp(p: &mut Psbt, i: usize, kp: &Keypair) -> anyhow::Result<()> {
    let prevout = p.inputs[i].witness_utxo.clone().ok_or_else(|| anyhow!("no witness utxo"))?;
    let (_, (script, ver)) = p.inputs[i].tap_scripts.first_key_value().ok_or_else(|| anyhow!("no tap script"))?;
    let leaf = TapLeafHash::from_script(script, *ver);
    let h = SighashCache::new(&p.unsigned_tx).taproot_script_spend_signature_hash(i, &Prevouts::One(i, &prevout), leaf, TapSighashType::SinglePlusAnyoneCanPay)?;
    let msg = secp256k1::Message::from_digest(h.to_raw_hash().to_byte_array());
    let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, kp);
    p.inputs[i].tap_script_sigs.insert((kp.x_only_public_key().0, leaf), taproot::Signature { signature: sig, sighash_type: TapSighashType::SinglePlusAnyoneCanPay });
    Ok(())
}

/// Everything the maker publishes. Nothing in it lets anyone take the asset without paying.
struct Offer {
    checkpoint: Psbt, // maker-signed (DEFAULT), carries T1
    template: Psbt,   // input 1 = CP:1 with the maker's SINGLE|ACP signature; output 1 = maker payment
                      // (RGB opret1st: the opret must be the first OP_RETURN-or-P2TR output, so it is
                      // output 0 and the maker's SINGLE pair sits at index 1; input 0 is a placeholder)
    t2_payload: [u8; 32],
}

/// Re-anchor: replace the bundled witness `from` (the maker's template) by the final tx.
pub(crate) fn reanchor(src: &Path, dst: &Path, from: Txid, to: &Transaction) -> anyhow::Result<usize> {
    let mut c = RgbTransfer::load_file(src).map_err(|e| anyhow!("load consignment: {e:?}"))?;
    let mut n = 0;
    for wb in c.bundles.iter_mut() {
        if wb.witness_id() == from {
            wb.pub_witness = PubWitness::with(to.clone());
            n += 1;
        }
    }
    c.save_file(dst).map_err(|e| anyhow!("save consignment: {e}"))?;
    Ok(n)
}

pub(crate) fn units(a: &[Assignment]) -> u64 {
    a.iter().map(|a| if let Assignment::Fungible(n) = a { *n } else { 0 }).sum()
}

pub(crate) async fn first(ark: &ArkClient, who: &str) -> anyhow::Result<OutPoint> {
    ark.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().next().map(|v| v.vtxo().outpoint).ok_or_else(|| anyhow!("{who} has no VTXO"))
}

/// Taker: the final ark tx = own input 0 + maker input 1; outputs [opret T2, maker, carrier, change, anchor].
fn assemble(offer: &Offer, own: &OffchainTransactions, b_sat: u64, buyer_spk: &ScriptBuf, opret_value: u64, payload: [u8; 32]) -> anyhow::Result<Psbt> {
    let t = &offer.template;
    let mut tx = t.unsigned_tx.clone();
    tx.input = vec![own.ark_tx.unsigned_tx.input[0].clone(), t.unsigned_tx.input[MK].clone()];
    let anchor = own.ark_tx.unsigned_tx.output.iter().find(|o| o.value == Amount::ZERO && !o.script_pubkey.is_op_return()).cloned().ok_or_else(|| anyhow!("no anchor"))?;
    let mut opret = bare_opret(Amount::from_sat(opret_value));
    opret.script_pubkey = ScriptBuf::new_op_return(payload);
    tx.output = vec![
        opret,
        t.unsigned_tx.output[MK].clone(),
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: buyer_spk.clone() },
        TxOut { value: Amount::from_sat(b_sat - PRICE_SAT - CARRIER_SAT - opret_value), script_pubkey: buyer_spk.clone() },
        anchor,
    ];
    let mut p = Psbt::from_unsigned_tx(tx)?;
    p.inputs = vec![own.ark_tx.inputs[0].clone(), t.inputs[MK].clone()];
    for i in p.inputs.iter_mut() {
        i.sighash_type = None; // arkd #58: the operator signs with this field; leave it DEFAULT
    }
    Ok(p)
}

pub async fn run(grief: Option<String>) -> anyhow::Result<()> {
    let run_id = match &grief { None => format!("run-{}-swap-ni", now()), Some(m) => format!("run-{}-swap-ni-grief-{m}", now()) };
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

    step("1. Alice (maker, single-key client) gets 1000 units on V; Bob (taker) and Carol board");
    let (alice, akp) = keypair_client("alice", regtest.clone(), &state).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    let (alice_c, _) = ark_client("alice-c", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    rec("arkd_version", info.version.clone());
    board("alice", &alice, "0.001").await?;
    board("bob", &bob, "0.001").await?;
    board("alice-c", &alice_c, "0.0002").await?;
    let h1 = height(&regtest)?;
    let (v, b) = (first(&alice, "alice").await?, first(&bob, "bob").await?);
    let c = first(&alice_c, "alice-c").await?;
    rec("vtxos", format!("V {v}, Alice spare C {c}, B {b} (batch 1 at height {h1})"));
    let (mut alice_rgb, alice_online) = bl(|| rgb_wallet("alice", &state))?;
    let (mut bob_rgb, bob_online) = bl(|| rgb_wallet("bob", &state))?;
    let (mut carol_rgb, carol_online) = bl(|| rgb_wallet("carol", &state))?;
    let (mut erin_rgb, erin_online) = bl(|| rgb_wallet("erin", &state))?;
    let (mut frank_rgb, frank_online) = bl(|| rgb_wallet("frank", &state))?;
    let asset_id = onchain_genesis_jump_in(&regtest, &mut alice_rgb, &alice_online, v, &state, &mut |k, v| println!("  >> {k} = {v}")).await?;
    let cid = ContractId::from_str(&asset_id).map_err(|e| anyhow!("{e}"))?;
    rec("asset_on_v", format!("{asset_id} on {v}"));

    step("2. Alice builds and signs the offer (sell 400 for 30,000 sat), then goes offline");
    let t0 = std::time::Instant::now();
    let s_in = alice.resolve_selected_send_inputs(&[v]).await?.remove(0);
    let s_keep = s_in.clone();
    let s_sat = s_in.amount().to_sat();
    let (alice_addr, _) = alice.get_offchain_address().await?;
    let recv = vec![SendReceiver::bitcoin(alice_addr.clone(), s_in.amount())];
    let dry = build_offchain_transactions(&recv, &alice_addr, &[s_in.clone().with_checkpoint_op_return(Some([0u8; 32]))], &info)?;
    let mut cp = dry.checkpoint_txs[0].clone();
    cp.unsigned_tx.output[0].script_pubkey = ScriptBuf::new_op_return([]);
    bl(|| alice_rgb.color_psbt_and_consume_custom(&mut cp, cid, vec![(SealTarget::WitnessVout(1), SOLD, BL_T1), (SealTarget::Explicit(Outpoint { txid: c.txid.to_string(), vout: c.vout }), ISSUE_AMOUNT - SOLD, BL_CHANGE)]))?;
    let mut t1 = [0u8; 32];
    t1.copy_from_slice(&cp.unsigned_tx.output[0].script_pubkey.as_bytes()[2..]);
    let OffchainTransactions { ark_tx: tmpl0, checkpoint_txs } = build_offchain_transactions(&recv, &alice_addr, &[s_in.with_checkpoint_op_return(Some(t1))], &info)?;
    let mut checkpoint = checkpoint_txs[0].clone();
    let cp_txid = checkpoint.unsigned_tx.compute_txid();
    ensure!(cp_txid == cp.unsigned_tx.compute_txid(), "checkpoint changed between the two builds");
    ensure!(tmpl0.unsigned_tx.input[0].previous_output == OutPoint { txid: cp_txid, vout: 1 }, "template does not spend CP:1");
    // template: inputs [placeholder, CP:1]; outputs [bare opret] [maker: V + price] [carrier placeholder]
    let mut tx = tmpl0.unsigned_tx.clone();
    tx.input = vec![TxIn::default(), tmpl0.unsigned_tx.input[0].clone()];
    tx.output = vec![
        bare_opret(Amount::from_sat(OPRET_SAT)),
        TxOut { value: Amount::from_sat(s_sat + PRICE_SAT), script_pubkey: alice_addr.to_p2tr_script_pubkey() },
        TxOut { value: Amount::from_sat(CARRIER_SAT), script_pubkey: alice_addr.to_p2tr_script_pubkey() },
    ];
    let mut template = Psbt::from_unsigned_tx(tx)?;
    template.inputs = vec![psbt::Input::default(), tmpl0.inputs[0].clone()];
    let mut colored = template.clone();
    let t2: RgbTransfer = bl(|| alice_rgb.color_psbt_and_consume_custom(&mut colored, cid, vec![(SealTarget::WitnessVout(2), SOLD, BL_BUYER)]))?;
    let mut t2_payload = [0u8; 32];
    t2_payload.copy_from_slice(&colored.unsigned_tx.output[0].script_pubkey.as_bytes()[2..]);
    template.unsigned_tx.output[0].script_pubkey = colored.unsigned_tx.output[0].script_pubkey.clone();
    let template_txid = template.unsigned_tx.compute_txid();
    sign_single_acp(&mut template, MK, &akp)?;
    sign_checkpoint_transaction(signer(akp), &mut checkpoint)?;
    let consignment = state.join("offer.rgbc");
    t2.save_file(&consignment).map_err(|e| anyhow!("save: {e}"))?;
    let offer_proof = proof_path(&consignment);
    build_proof(&alice, &regtest, &[], &[], &offer_proof).await?; // V is a batch VTXO: nothing unmined before it
    add_vtxos(&alice, &regtest, &offer_proof, &[v]).await?;
    let offer = Offer { checkpoint, template, t2_payload };
    let mut unsigned_cp = offer.checkpoint.clone();
    unsigned_cp.inputs[0].tap_script_sigs.clear();
    // the published offer: protobuf (proto/offers.proto); the JSON of the same content is only measured
    let offer_pb = crate::wire::SwapNiOffer {
        checkpoint_psbt_unsigned: unsigned_cp.serialize(),
        template_psbt: offer.template.serialize(),
        t2_opret: offer.t2_payload.to_vec(),
        sell_units: SOLD,
        price_sat: PRICE_SAT,
        asset: asset_id.clone(),
    };
    let offer_bytes = prost::Message::encode_to_vec(&offer_pb);
    std::fs::write(state.join("offer.pb"), &offer_bytes)?;
    let back: crate::wire::SwapNiOffer = prost::Message::decode(std::fs::read(state.join("offer.pb"))?.as_slice())?;
    ensure!(back == offer_pb && Psbt::deserialize(&back.template_psbt)? == offer.template, "offer.pb roundtrip");
    let legacy_json = serde_json::to_vec(&serde_json::json!({
        "checkpoint_psbt_unsigned": unsigned_cp.to_string(),
        "template_psbt": offer.template.to_string(),
        "t2_opret": hex::encode(offer.t2_payload),
        "sell_units": SOLD, "price_sat": PRICE_SAT, "asset": asset_id,
    }))?;
    rec("offer_build_ms", t0.elapsed().as_millis().to_string());
    rec("offer_pb_bytes", format!("{} (gzip {})", offer_bytes.len(), crate::wire::gz_len(&offer_bytes)));
    rec("offer_json_bytes", format!("{} (gzip {}) [same content as JSON, not written]", legacy_json.len(), crate::wire::gz_len(&legacy_json)));
    rec("offer_consignment_bytes", file_len(&consignment).to_string());
    rec("offer_arkproof_bytes", file_len(&offer_proof).to_string());
    rec("offer_arkproof_bytes_cmp", crate::proof::size_cmp(&offer_proof)?);
    rec("checkpoint_txid", cp_txid.to_string());
    rec("template_txid", template_txid.to_string());
    rec("template_outputs", format!("{:?}", outs(&offer.template)));
    println!("[alice] offer published; Alice is offline from here (her client is not used until step 8)");

    step("3. Bob checks the offer offline (template witness) before building anything");
    let pre = bl(|| bob_rgb.validate_consignment_offchain(bob_online.clone(), consignment.clone(), vec![cp_txid.to_string(), template_txid.to_string()], template_txid.to_string(), 2))?;
    rec("bob_offer_check_units_vout2", units(&pre).to_string());
    ensure!(units(&pre) == SOLD, "offer gives {} on vout 2", units(&pre));

    step("4. Bob builds his side");
    let b_in = bob.resolve_selected_send_inputs(&[b]).await?.remove(0);
    let b_sat = b_in.amount().to_sat();
    let (bob_addr, _) = bob.get_offchain_address().await?;
    let bob_spk = bob_addr.to_p2tr_script_pubkey();
    let own = build_offchain_transactions(&[SendReceiver::bitcoin(bob_addr.clone(), b_in.amount())], &bob_addr, &[b_in], &info)?;
    // submit never needs user checkpoint signatures (arkd re-builds and signs the checkpoints)
    let mut public_cp = offer.checkpoint.clone();
    public_cp.inputs[0].tap_script_sigs.clear();
    let cps = vec![public_cp, own.checkpoint_txs[0].clone()];

    let submit = |p: Psbt| {
        let cps = cps.clone();
        let bob = &bob;
        async move { bob.network_client().submit_offchain_transaction_request(p, cps).await.map_err(|e| format!("{e:?}").chars().take(400).collect::<String>()) }
    };

    if let Some(mode) = grief {
        return grief_run(&mode, &mut rec, &regtest, &info, &alice, &mut alice_rgb, cid, &asset_id, &alice_online, &bob, &offer, &own, b_sat, &bob_spk, &cps, v, h1, cp_txid, s_keep, &state).await;
    }

    step("5a. negative: Bob underpays Alice by 1 sat");
    let mut bad = assemble(&offer, &own, b_sat, &bob_spk, OPRET_SAT, offer.t2_payload)?;
    bad.unsigned_tx.output[MK].value = Amount::from_sat(s_sat + PRICE_SAT - 1);
    bad.unsigned_tx.output[3].value += Amount::from_sat(1);
    bob.sign_ark_tx_inputs(&mut bad, &[0]).map_err(|e| anyhow!("{e:?}"))?;
    let r = submit(bad).await;
    rec("neg_underpay", format!("{:?}", r.as_ref().err()));
    ensure!(r.is_err(), "arkd accepted an underpaying fill");

    step("5b. negative: Bob redirects Alice's output 1 to himself");
    let mut bad = assemble(&offer, &own, b_sat, &bob_spk, OPRET_SAT, offer.t2_payload)?;
    bad.unsigned_tx.output[MK].script_pubkey = bob_spk.clone();
    bob.sign_ark_tx_inputs(&mut bad, &[0]).map_err(|e| anyhow!("{e:?}"))?;
    let r = submit(bad).await;
    rec("neg_redirect", format!("{:?}", r.as_ref().err()));
    ensure!(r.is_err(), "arkd accepted a redirected fill");

    step("5c. negative (#58): the PSBT maker-input sighash field set to SINGLE|ACP: the operator must refuse to sign");
    let mut bad = assemble(&offer, &own, b_sat, &bob_spk, OPRET_SAT, offer.t2_payload)?;
    bad.inputs[MK].sighash_type = Some(PsbtSighashType::from(TapSighashType::SinglePlusAnyoneCanPay));
    bob.sign_ark_tx_inputs(&mut bad, &[0]).map_err(|e| anyhow!("{e:?}"))?;
    let r = submit(bad).await;
    rec("neg_psbt_sighash_field", format!("{:?}", r.as_ref().err()));
    ensure!(r.is_err(), "arkd signed with a client-chosen SINGLE|ACP");

    step("6. Bob fills through the maker's fill agent: re-anchors and checks T2, signs the ark tx AND presigns his own checkpoint; the agent submits + finalizes");
    let (agent, _) = ark_client("agent", regtest.clone(), &state).await?;
    let t_fill = std::time::Instant::now();
    let mut fin = assemble(&offer, &own, b_sat, &bob_spk, OPRET_SAT, offer.t2_payload)?;
    let final_txid = fin.unsigned_tx.compute_txid();
    rec("final_txid", final_txid.to_string());
    rec("final_outputs", format!("{:?}", outs(&fin)));
    // the offer consignment does not validate on the final tx as is ...
    let stale = bl(|| bob_rgb.validate_consignment_offchain(bob_online.clone(), consignment.clone(), vec![cp_txid.to_string(), final_txid.to_string()], final_txid.to_string(), 2));
    rec("stale_consignment_on_final", format!("{:?}", stale.as_ref().map(|a| units(a)).map_err(|e| e.to_string())));
    // ... re-anchored it does, byte-identical transitions
    let fin_cons = state.join("filled.rgbc");
    let n = reanchor(&consignment, &fin_cons, template_txid, &fin.unsigned_tx)?;
    rec("reanchored_bundles", n.to_string());
    let got = bl(|| bob_rgb.validate_consignment_offchain(bob_online.clone(), fin_cons.clone(), vec![cp_txid.to_string(), final_txid.to_string()], final_txid.to_string(), 2))?;
    rec("bob_check_final_vout2", units(&got).to_string());
    ensure!(units(&got) == SOLD, "re-anchored consignment gives {}", units(&got));
    bob.sign_ark_tx_inputs(&mut fin, &[0]).map_err(|e| anyhow!("{e:?}"))?;
    let mut bob_cp = own.checkpoint_txs[0].clone();
    if bob_cp.inputs[0].witness_script.is_none() {
        bob_cp.inputs[0].witness_script = own.ark_tx.inputs[0].witness_script.clone();
    }
    bob.sign_checkpoint_tx(&mut bob_cp).map_err(|e| anyhow!("{e:?}"))?;
    let bob_cp_txid = bob_cp.unsigned_tx.compute_txid();
    // agent side: only the maker's agent knows the maker's checkpoint signature; it checks the fill
    // pays the maker and that the taker's checkpoint is already signed, then submits AND finalizes
    ensure!(fin.unsigned_tx.output[MK] == offer.template.unsigned_tx.output[MK], "fill does not pay the maker");
    ensure!(!bob_cp.inputs[0].tap_script_sigs.is_empty(), "taker checkpoint not presigned");
    let res = agent.network_client().submit_offchain_transaction_request(fin, cps.clone()).await.map_err(|e| anyhow!("submit: {e:?}"))?;
    ensure!(res.signed_ark_tx.unsigned_tx.compute_txid() == final_txid, "arkd changed the ark tx");
    let mut signed_cps = res.signed_checkpoint_txs;
    for c in signed_cps.iter_mut() {
        let txid = c.unsigned_tx.compute_txid();
        let idx = res.signed_ark_tx.unsigned_tx.input.iter().position(|i| i.previous_output.txid == txid).ok_or_else(|| anyhow!("checkpoint {txid} not spent"))?;
        if c.inputs[0].witness_script.is_none() {
            c.inputs[0].witness_script = res.signed_ark_tx.inputs[idx].witness_script.clone();
        }
        let before = c.inputs[0].tap_script_sigs.len();
        if txid == cp_txid {
            c.inputs[0].tap_script_sigs.extend(offer.checkpoint.inputs[0].tap_script_sigs.clone());
        } else {
            ensure!(txid == bob_cp_txid, "unexpected checkpoint {txid}");
            c.inputs[0].tap_script_sigs.extend(bob_cp.inputs[0].tap_script_sigs.clone());
        }
        println!("checkpoint {txid}: sigs {before} -> {}", c.inputs[0].tap_script_sigs.len());
    }
    agent.network_client().finalize_offchain_transaction(final_txid, signed_cps).await.map_err(|e| anyhow!("finalize: {e:?}"))?;
    rec("fill_ms", t_fill.elapsed().as_millis().to_string());
    let bob_v = OutPoint { txid: final_txid, vout: 2 };
    let alice_v = OutPoint { txid: final_txid, vout: MK as u32 };
    wait_vtxo(&bob, bob_v, "bob").await?;
    wait_vtxo(&alice, alice_v, "alice").await?;
    let alice_sat = alice.list_vtxos_filtered(GetVtxosRequestFilter::Spendable).await?.spendable_offchain().find(|x| x.vtxo().outpoint == alice_v).map(|x| x.vtxo().amount.to_sat()).unwrap_or(0);
    rec("alice_paid_vtxo_sat", alice_sat.to_string());
    ensure!(alice_sat == s_sat + PRICE_SAT, "Alice got {alice_sat}");

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

    step("8. Alice back online: her 600 change on her spare VTXO C validates (T1, fixed by her checkpoint signature)");
    import(&alice_rgb, &alice_online, &pp, &ops)?;
    let ch = bl(|| alice_rgb.validate_consignment_offchain(alice_online.clone(), fin_cons.clone(), vec![], c.txid.to_string(), c.vout))?;
    rec("alice_change_on_c", units(&ch).to_string());
    ensure!(units(&ch) == ISSUE_AMOUNT - SOLD, "Alice change {}", units(&ch));

    step("9. Alice registers her 600 change on C (tracked by the imported Ark proof)");
    let reg_c = bl(|| alice_rgb.ark_receive_vtxo_transfer(alice_online.clone(), fin_cons.clone(), c.to_string())).map_err(|e| anyhow!("{e}"));
    rec("alice_registers_change_on_c", format!("{:?}", reg_c.as_ref().map_err(|e| e.to_string())));

    step("10. a later batch (240 blocks on): fresh VTXOs for the renewals and the receivers");
    regtest::mine(240)?;
    let (alice_g, _) = ark_client("alice-g", regtest.clone(), &state).await?;
    let (bob_g, _) = ark_client("bob-g", regtest.clone(), &state).await?;
    let (carol, _) = ark_client("carol", regtest.clone(), &state).await?;
    let (erin, _) = ark_client("erin", regtest.clone(), &state).await?;
    board("alice-g", &alice_g, "0.0005").await?;
    board("bob-g", &bob_g, "0.0005").await?;
    board("carol", &carol, "0.0002").await?;
    board("erin", &erin, "0.0002").await?;
    let h2 = height(&regtest)?;
    let (ga, gb, rv, ev) = (first(&alice_g, "alice-g").await?, first(&bob_g, "bob-g").await?, first(&carol, "carol").await?, first(&erin, "erin").await?);
    let exp = |x: Option<ark_core::server::VirtualTxOutPoint>| x.map(|x| x.expires_at).unwrap_or_default();
    rec("batch2", format!("height {h2}: Ga {ga} (exp {}), Gb {gb} (exp {}), Carol R {rv}, Erin E {ev}; final:2 exp {}, C exp {}",
        exp(crate::long_offline::vtxo_info(ga).await), exp(crate::long_offline::vtxo_info(gb).await),
        exp(crate::long_offline::vtxo_info(bob_v).await), exp(crate::long_offline::vtxo_info(c).await)));

    step("11. renewals into batch 2: Bob final:2 -> Gb (400), Alice C -> Ga (600), Alice's paid bitcoin settled");
    let (cp_rb, _, ark_rb) = checkpoint_hop(&bob, &bob_rgb, &info, bob_v, cid, SOLD, gb, 9021).await?;
    rec("renew_bob", format!("checkpoint {cp_rb}, ark tx {ark_rb}"));
    let (cp_ra, _, ark_ra) = checkpoint_hop(&alice_c, &alice_rgb, &info, c, cid, ISSUE_AMOUNT - SOLD, ga, 9022).await?;
    rec("renew_alice_change", format!("checkpoint {cp_ra}, ark tx {ark_ra}"));
    let st = alice.settle_all(&mut rand::thread_rng()).await;
    rec("alice_btc_settle", format!("{:?}", st.as_ref().map_err(|e| format!("{e:?}").chars().take(300).collect::<String>())));

    step("12. batch 1 expires (mine past its tree expiry) and arkd sweeps it");
    mine_to(&regtest, h1 + TREE_EXPIRY + 20)?;
    let left = OutPoint { txid: ark_rb, vout: 0 };
    let mut swept = false;
    for _ in 0..40 {
        if crate::long_offline::vtxo_info(left).await.is_some_and(|x| x.is_swept) {
            swept = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    rec("batch1_swept", format!("height {}: {left} (bitcoin left on batch 1 by Bob's renewal) swept {swept}", height(&regtest)?));
    ensure!(swept, "batch 1 not swept");

    step("13. after the renewals: Bob sends 400 Gb -> Carol R; Alice (maker) spends her 600 change Ga -> Erin E; both validate offline");
    let (cp_bc, to_carol_t, ark_bc) = checkpoint_hop(&bob_g, &bob_rgb, &info, gb, cid, SOLD, rv, 9031).await?;
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
    let (cp_ae, to_erin_t, ark_ae) = checkpoint_hop(&alice_g, &alice_rgb, &info, ga, cid, ISSUE_AMOUNT - SOLD, ev, 9032).await?;
    rec("alice_change_to_erin", format!("checkpoint {cp_ae}, ark tx {ark_ae}"));
    let to_erin = state.join("to-erin.rgbc");
    to_erin_t.save_file(&to_erin).map_err(|e| anyhow!("save: {e}"))?;
    let pe = proof_path(&to_erin);
    let ste = build_proof(&alice_g, &regtest, &[cp_txid, cp_ra, cp_ae], &[], &pe).await?;
    add_vtxos(&erin, &regtest, &pe, &[ev]).await?;
    rec("erin_proof", format!("{} bytes, {} psbts; consignment {} bytes", ste.bytes, ste.n_psbts, file_len(&to_erin)));
    {
        let _down = arkd_down(&mut rec, "erin")?;
        import(&erin_rgb, &erin_online, &pe, &ops)?;
        let got = bl(|| erin_rgb.ark_receive_vtxo_transfer(erin_online.clone(), to_erin.clone(), ev.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("erin_received", got.to_string());
        ensure!(got == ISSUE_AMOUNT - SOLD, "Erin received {got}");
    }

    step("14. a new batch and a further send");
    let (frank, _) = ark_client("frank", regtest.clone(), &state).await?;
    board("frank", &frank, "0.0002").await?;
    let fv = first(&frank, "frank").await?;
    regtest::mine(1)?; // the batch commitment must be mined before the proof treats it as on-chain
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    rec("frank_batch", fv.to_string());
    let (cp_cf, to_frank_t, ark_cf) = checkpoint_hop(&carol, &carol_rgb, &info, rv, cid, SOLD, fv, 9041).await?;
    rec("carol_to_frank", format!("checkpoint {cp_cf}, ark tx {ark_cf}"));
    let to_frank = state.join("to-frank.rgbc");
    to_frank_t.save_file(&to_frank).map_err(|e| anyhow!("save: {e}"))?;
    let pf = proof_path(&to_frank);
    let stf = build_proof(&carol, &regtest, &[cp_txid, final_txid, cp_rb, cp_bc, cp_cf], &[], &pf).await?;
    add_vtxos(&frank, &regtest, &pf, &[fv]).await?;
    rec("frank_proof", format!("{} bytes, {} psbts; consignment {} bytes", stf.bytes, stf.n_psbts, file_len(&to_frank)));
    {
        let _down = arkd_down(&mut rec, "frank")?;
        import(&frank_rgb, &frank_online, &pf, &ops)?;
        let got = bl(|| frank_rgb.ark_receive_vtxo_transfer(frank_online.clone(), to_frank.clone(), fv.to_string())).map_err(|e| anyhow!("{e}"))?;
        rec("frank_received", got.to_string());
        ensure!(got == SOLD, "Frank received {got}");
    }
    println!("SWAP-NI RESULT: PASS");
    Ok(())
}

const TREE_EXPIRY: u64 = 500; // cponly profile, blocks

pub(crate) fn height(regtest: &Regtest) -> anyhow::Result<u64> {
    regtest.rpc("getblockcount", serde_json::json!([]))?.as_u64().ok_or_else(|| anyhow!("getblockcount"))
}

pub(crate) fn mine_to(regtest: &Regtest, h: u64) -> anyhow::Result<()> {
    let now_h = height(regtest)?;
    if h > now_h {
        regtest::mine((h - now_h) as u32)?;
    }
    Ok(())
}

/// Exit branch of any VTXO (also a spent / pending one, which the client's own exit builder skips).
pub(crate) async fn branch_any(client: &ArkClient, regtest: &Regtest, v: OutPoint) -> anyhow::Result<Vec<Transaction>> {
    use ark_core::unilateral_exit::{UnilateralExitTree, build_unilateral_exit_tree_txids, finalize_unilateral_exit_tree};
    let vi = crate::long_offline::vtxo_info(v).await.ok_or_else(|| anyhow!("no info for {v}"))?;
    let nc = client.network_client();
    let chain = nc.get_vtxo_chain(Some(v), None, None, None).await.map_err(|e| anyhow!("vtxo chain: {e:?}"))?;
    let paths = build_unilateral_exit_tree_txids(&chain.chains, v.txid)?;
    let ids: std::collections::HashSet<Txid> = paths.concat().into_iter().collect();
    let vt = nc.get_virtual_txs(ids.iter().map(|t| t.to_string()).collect(), None, None).await.map_err(|e| anyhow!("virtual txs: {e:?}"))?;
    let paths = paths
        .into_iter()
        .map(|p| p.into_iter().map(|id| vt.txs.iter().find(|t| t.unsigned_tx.compute_txid() == id).cloned().ok_or_else(|| anyhow!("no psbt {id}"))).collect::<anyhow::Result<Vec<_>>>())
        .collect::<anyhow::Result<Vec<_>>>()?;
    let tree = UnilateralExitTree::new(vi.commitment_txids.clone(), paths);
    let mut ctxs = vec![];
    for c in tree.commitment_txids() {
        let hex = regtest.rpc("getrawtransaction", serde_json::json!([c.to_string()]))?;
        ctxs.push(bitcoin::consensus::encode::deserialize_hex::<Transaction>(hex.as_str().ok_or_else(|| anyhow!("raw tx"))?)?);
    }
    finalize_unilateral_exit_tree(&tree, &ctxs)?.into_iter().find(|b| b.last().map(|t| t.compute_txid()) == Some(v.txid)).ok_or_else(|| anyhow!("no branch to {v}"))
}

/// Spender of `op` in the mempool or the last `blocks` blocks.
pub(crate) fn find_spender(regtest: &Regtest, op: OutPoint, blocks: u64) -> anyhow::Result<Option<serde_json::Value>> {
    let matches = |tx: &serde_json::Value| tx["vin"].as_array().is_some_and(|v| v.iter().any(|i| i["txid"].as_str() == Some(&op.txid.to_string()) && i["vout"].as_u64() == Some(op.vout as u64)));
    for t in regtest.rpc("getrawmempool", serde_json::json!([]))?.as_array().cloned().unwrap_or_default() {
        let tx = regtest.rpc("getrawtransaction", serde_json::json!([t, true]))?;
        if matches(&tx) {
            return Ok(Some(tx));
        }
    }
    let h = height(regtest)?;
    for k in 0..blocks {
        let hash = regtest.rpc("getblockhash", serde_json::json!([h - k]))?;
        let b = regtest.rpc("getblock", serde_json::json!([hash, 2]))?;
        for tx in b["tx"].as_array().cloned().unwrap_or_default() {
            if matches(&tx) {
                return Ok(Some(tx));
            }
        }
    }
    Ok(None)
}

fn arkd_log_lines(since: &str, needles: &[&str]) -> Vec<String> {
    let Ok(o) = std::process::Command::new("docker").args(["logs", "--since", since, "arkd"]).output() else { return vec![] };
    let all = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    all.lines().filter(|l| needles.iter().any(|n| l.contains(n))).map(|l| l.chars().take(300).collect()).collect()
}

/// Griefing: Bob submits a valid fill and never finalizes. Then `mode`:
/// exit = the maker exits V unilaterally; burn = the taker puts V and the maker's (now fully
/// signed) checkpoint on-chain; expire = nobody acts until V's batch expires.
#[allow(clippy::too_many_arguments)]
async fn grief_run(
    mode: &str,
    rec: &mut dyn FnMut(&str, String),
    regtest: &Regtest,
    info: &ark_core::server::Info,
    alice: &ArkClient,
    alice_rgb: &mut rgb_lib::wallet::Wallet,
    cid: ContractId,
    asset_id: &str,
    alice_online: &rgb_lib::wallet::Online,
    bob: &ArkClient,
    offer: &Offer,
    own: &OffchainTransactions,
    b_sat: u64,
    bob_spk: &ScriptBuf,
    cps: &[Psbt],
    v: OutPoint,
    h1: u64,
    cp_txid: Txid,
    s_keep: ark_core::send::VtxoInput,
    state: &Path,
) -> anyhow::Result<()> {
    step("G1. Bob submits a valid fill and never finalizes");
    let mut fin = assemble(offer, own, b_sat, bob_spk, OPRET_SAT, offer.t2_payload)?;
    let fill_txid = fin.unsigned_tx.compute_txid();
    bob.sign_ark_tx_inputs(&mut fin, &[0]).map_err(|e| anyhow!("{e:?}"))?;
    let res = bob.network_client().submit_offchain_transaction_request(fin, cps.to_vec()).await.map_err(|e| anyhow!("submit: {e:?}"))?;
    rec("grief_fill_submitted", format!("{fill_txid} at height {}", height(regtest)?));
    let stored_cp = res.signed_checkpoint_txs.iter().find(|c| c.unsigned_tx.compute_txid() == cp_txid).cloned().ok_or_else(|| anyhow!("maker checkpoint not returned"))?;
    let maker_key = offer.checkpoint.inputs[0].tap_script_sigs.keys().next().map(|k| k.0);
    rec("operator_signed_maker_checkpoint_has_maker_sig", format!("{}", stored_cp.inputs[0].tap_script_sigs.keys().any(|k| Some(k.0) == maker_key)));
    let vi = crate::long_offline::vtxo_info(v).await;
    rec("v_after_submit", format!("{:?}", vi.as_ref().map(|x| (x.is_spent, x.spent_by, x.ark_txid, x.expires_at))));

    step("G2. the maker tries to cancel (spend V to itself); retried every 60 s for SWAPNI_LOCK_MIN minutes");
    let (alice_addr, _) = alice.get_offchain_address().await?;
    let cancel = || -> anyhow::Result<(Psbt, Vec<Psbt>)> {
        let o = build_offchain_transactions(&[SendReceiver::bitcoin(alice_addr.clone(), s_keep.amount())], &alice_addr, &[s_keep.clone()], info)?;
        let mut a = o.ark_tx;
        alice.sign_ark_tx_inputs(&mut a, &[0]).map_err(|e| anyhow!("{e:?}"))?;
        Ok((a, o.checkpoint_txs))
    };
    let lock_min: u64 = std::env::var("SWAPNI_LOCK_MIN").ok().and_then(|x| x.parse().ok()).unwrap_or(0);
    let t0 = std::time::Instant::now();
    let mut n = 0;
    let mut released = None;
    let mut last = String::new();
    loop {
        n += 1;
        let (a, ac) = cancel()?;
        match alice.network_client().submit_offchain_transaction_request(a, ac).await {
            Ok(_) => {
                released = Some(t0.elapsed().as_secs());
                break;
            }
            Err(e) => last = format!("{e:?}").chars().take(200).collect(),
        }
        if t0.elapsed().as_secs() >= lock_min * 60 {
            break;
        }
        if n % 10 == 0 {
            println!("[grief] still locked after {} s ({n} attempts, height {})", t0.elapsed().as_secs(), height(regtest)?);
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
    rec("cancel_attempts", format!("{n} over {} s (height {}); released after: {released:?}; last error: {last}", t0.elapsed().as_secs(), height(regtest)?));
    if released.is_some() {
        rec("grief_result", "lock released: the maker's cancel went through".into());
        println!("SWAP-NI-GRIEF RESULT: PASS");
        return Ok(());
    }

    step("G3. can the maker finalize the stuck fill itself? (it holds its checkpoint signature, not Bob's)");
    let pend = alice.list_pending_offchain_txs().await;
    rec("maker_sees_pending", format!("{:?}", pend.as_ref().map(|p| p.iter().map(|x| x.ark_txid).collect::<Vec<_>>()).map_err(|e| format!("{e:?}").chars().take(200).collect::<String>())));
    let f = alice.finalize_pending_offchain_tx(fill_txid).await;
    rec("maker_finalize", format!("{:?}", f.as_ref().map_err(|e| format!("{e:?}").chars().take(400).collect::<String>())));
    if f.is_ok() {
        rec("grief_result", "the maker could finalize the fill itself".into());
        println!("SWAP-NI-GRIEF RESULT: PASS");
        return Ok(());
    }

    match mode {
        "exit" => {
            step("G4 (exit). the maker unrolls V on-chain; does arkd answer with the checkpoint? then the maker spends V's exit path");
            let branch = branch_any(alice, regtest, v).await?;
            rec("v_branch", format!("{:?}", branch.iter().map(|t| t.compute_txid()).collect::<Vec<_>>()));
            let mut wt = Watchtower::new(regtest).await?;
            unroll(&mut wt, regtest, &branch, &mut *rec).await?;
            let hv = height(regtest)?;
            rec("v_onchain", format!("height {hv}"));
            for _ in 0..6 {
                regtest::mine(1)?;
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            let sp = find_spender(regtest, v, 8)?;
            rec("v_spent_by_after_6_blocks", format!("{:?}", sp.as_ref().map(|t| t["txid"].clone())));
            rec("arkd_checkpoint_log", format!("{:?}", arkd_log_lines("10m", &["checkpoint", "fraud"]).into_iter().rev().take(6).collect::<Vec<_>>()));
            let (mut gina_rgb, gina_online) = bl(|| rgb_wallet("gina", state))?;
            let recv = bl(|| gina_rgb.witness_receive(None, Assignment::Fungible(ISSUE_AMOUNT), now() + 86_400, vec![], 1))?;
            let gscript = rgb_lib::utils::script_buf_from_recipient_id(recv.recipient_id.clone())?.ok_or_else(|| anyhow!("not witness"))?;
            let gaddr = bitcoin::Address::from_script(&gscript, Network::Regtest)?;
            let to_gina = state.join("v-exit-to-gina.rgbc");
            let r = spend_exited("alice", alice, alice_rgb, regtest, v, &gaddr, cid, ISSUE_AMOUNT, 9051, &to_gina).await;
            rec("maker_exit_spend", format!("{:?}", r.as_ref().map_err(|e| e.to_string())));
            if r.is_err() {
                rec("rgb_fail_transfers", format!("{:?}", bl(|| alice_rgb.fail_transfers(alice_online.clone(), None, false, false)).map_err(|e| e.to_string())));
                let r2 = spend_exited("alice", alice, alice_rgb, regtest, v, &gaddr, cid, ISSUE_AMOUNT, 9052, &to_gina).await;
                rec("maker_exit_spend_retry", format!("{:?}", r2.as_ref().map_err(|e| e.to_string())));
                r2?;
            }
            regtest::mine(1)?;
            let plain = bl(|| gina_rgb.validate_consignment_ark(gina_online.clone(), to_gina.clone(), false));
            rec("gina_plain_rgb_validation", format!("{:?}", plain.as_ref().map(|_| "valid").map_err(|e| e.to_string())));
            crate::checkpoint_offline::settle(&mut gina_rgb, &gina_online, &to_gina, asset_id, &mut *rec).await?;
        }
        "burn" => {
            step("G4 (burn). Bob puts V and the maker's checkpoint (maker sig from the offer + operator sig from SubmitTx) on-chain, then walks away");
            let mut mcp = stored_cp.clone();
            mcp.inputs[0].tap_script_sigs.extend(offer.checkpoint.inputs[0].tap_script_sigs.clone());
            if mcp.inputs[0].tap_scripts.is_empty() {
                mcp.inputs[0].tap_scripts = offer.checkpoint.inputs[0].tap_scripts.clone();
            }
            let wu = offer.checkpoint.inputs[0].witness_utxo.clone().ok_or_else(|| anyhow!("no witness utxo"))?;
            let cptx = ark_core::unilateral_exit::finalize_virtual_tx_input(mcp, 0, wu)?;
            let branch = branch_any(bob, regtest, v).await?;
            let mut wt = Watchtower::new(regtest).await?;
            unroll(&mut wt, regtest, &branch, &mut *rec).await?;
            let (child, fee) = wt.bump(&cptx)?;
            let r = crate::exit_watchtower::submit_package(regtest, &[&cptx, &child])?;
            rec("checkpoint_broadcast", format!("{} fee {fee}: {}", cptx.compute_txid(), r["package_msg"]));
            regtest::mine(1)?;
            let hc = height(regtest)?;
            rec("checkpoint_confirmed", format!("{:?} at height {hc}", regtest.tx_confirmations(&cp_txid)));
            let cp1 = OutPoint { txid: cp_txid, vout: 1 };
            let mut spender = None;
            for _ in 0..30 {
                regtest::mine(1)?;
                tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                if let Some(t) = find_spender(regtest, cp1, 3)? {
                    spender = Some(t);
                    break;
                }
            }
            let hs = height(regtest)?;
            rec("cp1_spender", format!("{:?} at height {hs} ({} blocks after the checkpoint)", spender.as_ref().map(|t| (t["txid"].clone(), t["vout"].as_array().map(|o| o.iter().map(|x| (x["value"].clone(), x["scriptPubKey"]["address"].clone())).collect::<Vec<_>>()))), hs - hc));
            rec("arkd_sweep_log", format!("{:?}", arkd_log_lines("15m", &["sweep", "checkpoint"]).into_iter().rev().take(8).collect::<Vec<_>>()));
            ensure!(spender.is_some(), "CP:1 not spent within 30 blocks");
        }
        _ => {
            step("G4 (expire). nobody acts; mine past V's batch expiry");
            mine_to(regtest, h1 + TREE_EXPIRY + 20)?;
            let mut last = None;
            for _ in 0..40 {
                last = crate::long_offline::vtxo_info(v).await;
                if last.as_ref().is_some_and(|x| x.is_swept) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            rec("v_after_expiry", format!("height {}: {:?}", height(regtest)?, last.as_ref().map(|x| (x.is_spent, x.is_swept, x.is_unrolled))));
            let b = crate::long_offline::vtxo_info(own.ark_tx.unsigned_tx.input[0].previous_output).await;
            rec("bob_checkpoint_input_after_expiry", format!("{:?}", b.as_ref().map(|x| (x.is_spent, x.is_swept))));
            // Bob comes back after the expiry and finalizes properly: is the maker paid?
            let mut signed_cps = res.signed_checkpoint_txs.clone();
            for c in signed_cps.iter_mut() {
                let txid = c.unsigned_tx.compute_txid();
                let idx = res.signed_ark_tx.unsigned_tx.input.iter().position(|i| i.previous_output.txid == txid).ok_or_else(|| anyhow!("checkpoint {txid} not spent"))?;
                if c.inputs[0].witness_script.is_none() {
                    c.inputs[0].witness_script = res.signed_ark_tx.inputs[idx].witness_script.clone();
                }
                if txid == cp_txid {
                    c.inputs[0].tap_script_sigs.extend(offer.checkpoint.inputs[0].tap_script_sigs.clone());
                } else {
                    bob.sign_checkpoint_tx(c).map_err(|e| anyhow!("{e:?}"))?;
                }
            }
            let f2 = bob.network_client().finalize_offchain_transaction(fill_txid, signed_cps).await;
            rec("late_finalize_after_expiry", format!("{:?}", f2.as_ref().map_err(|e| format!("{e:?}").chars().take(300).collect::<String>())));
            let paid = crate::long_offline::vtxo_info(OutPoint { txid: fill_txid, vout: MK as u32 }).await;
            rec("maker_payment_vtxo_after_late_finalize", format!("{:?}", paid.as_ref().map(|x| (x.amount, x.is_spent, x.is_swept, x.expires_at))));
        }
    }
    println!("SWAP-NI-GRIEF RESULT: PASS");
    Ok(())
}

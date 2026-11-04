// Cross-check: rgb-consensus verifies a tapret commitment on an Ark VTXO
// output key computed by the Go probe (default VTXO: CSV exit + user/operator
// multisig, NUMS internal key, commitment leaf at depth 1).
use std::str::FromStr;
use bitcoin::hashes::Hash;
use bitcoin::key::UntweakedPublicKey;
use bitcoin::{absolute, transaction, Amount, ScriptBuf, TapNodeHash, Transaction, TxOut};
use rgbcore::commit_verify::mpc;
use rgbcore::dbc::tapret::{TapretNodePartner, TapretPathProof, TapretProof};
use rgbcore::dbc::Proof;

fn h(s: &str) -> TapNodeHash {
    let mut b = [0u8; 32];
    for i in 0..32 { b[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap(); }
    TapNodeHash::from_byte_array(b)
}

fn main() {
    let a = h("15ddb35020c20391930e7edf356d3fe9926c5c6d40b560e6145d8b5ee1e0ef94");
    let b = h("d40a5a16fb1c63f1efde15e7548162f773599b1d9b4ff44ff336e4f5246ea6a5");
    let internal_pk = UntweakedPublicKey::from_str("50929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0").unwrap();
    let out_key = bitcoin::key::XOnlyPublicKey::from_str("0cde404e9e149257e3e5e1cbdcb07caaf19cc6dedd3bb0bd80f30d7b807ebe21").unwrap();
    let proof = TapretProof {
        path_proof: TapretPathProof::with(TapretNodePartner::right_branch(a, b), 0).unwrap(),
        internal_pk,
    };
    let spk = ScriptBuf::new_p2tr_tweaked(bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(out_key));
    let anchor = ScriptBuf::from_bytes(vec![0x51, 0x02, 0x4e, 0x73]); // P2A
    let tx = Transaction {
        version: transaction::Version(3),
        lock_time: absolute::LockTime::ZERO,
        input: vec![],
        output: vec![TxOut { value: Amount::from_sat(10_000), script_pubkey: spk }, TxOut { value: Amount::ZERO, script_pubkey: anchor }],
    };
    let msg = mpc::Commitment::from([0xabu8; 32]);
    println!("rgb-consensus TapretProof::verify on Go-built VTXO key: {:?}", proof.verify(&msg, &tx));
    let wrong = mpc::Commitment::from([0xacu8; 32]);
    println!("same proof, other commitment: {:?}", proof.verify(&wrong, &tx));
    println!("original (untweaked) VTXO spk restored by proof: {}", proof.original_pubkey_script());
}

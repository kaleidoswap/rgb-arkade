//! Protobuf messages of `proto/offers.proto` (prost derives mirroring the .proto; field numbers
//! are the contract, `scripts/proto-check.sh` cross-checks them). The Ark proof messages live in
//! rgb-lib (`rgb_lib::ark_proof::pb`, `proto/ark_proof.proto`).
use rgb_lib::ark_proof::pb::OutPoint;

/// `rgbarkade.v1.SwapNiOffer`
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SwapNiOffer {
    #[prost(bytes = "vec", tag = "1")]
    pub checkpoint_psbt_unsigned: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub template_psbt: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub t2_opret: Vec<u8>,
    #[prost(uint64, tag = "4")]
    pub sell_units: u64,
    #[prost(uint64, tag = "5")]
    pub price_sat: u64,
    #[prost(string, tag = "6")]
    pub asset: String,
}

/// `rgbarkade.v1.SwapEmuOffer`
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SwapEmuOffer {
    #[prost(message, optional, tag = "1")]
    pub v: Option<OutPoint>,
    #[prost(bytes = "vec", tag = "2")]
    pub covenant: Vec<u8>,
    #[prost(uint32, tag = "3")]
    pub cancel_height: u32,
    #[prost(bytes = "vec", tag = "4")]
    pub maker_xonly: Vec<u8>,
    #[prost(bytes = "vec", tag = "5")]
    pub emulator_pubkey: Vec<u8>,
    #[prost(bytes = "vec", tag = "6")]
    pub c1: Vec<u8>,
    #[prost(bytes = "vec", tag = "7")]
    pub c2: Vec<u8>,
    #[prost(bytes = "vec", tag = "8")]
    pub maker_sig: Vec<u8>,
    #[prost(bytes = "vec", tag = "9")]
    pub template_psbt: Vec<u8>,
    #[prost(uint64, tag = "10")]
    pub sell_units: u64,
    #[prost(uint64, tag = "11")]
    pub price_sat: u64,
    #[prost(string, tag = "12")]
    pub asset: String,
}

pub(crate) fn outpoint(o: &bitcoin::OutPoint) -> OutPoint {
    use bitcoin::hashes::Hash;
    OutPoint { txid: o.txid.to_byte_array().to_vec(), vout: o.vout }
}

/// gzip -9 size of `b` (for the size comparisons).
pub(crate) fn gz_len(b: &[u8]) -> u64 {
    use std::io::Write;
    let Ok(mut c) = std::process::Command::new("gzip").args(["-9", "-c"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn() else { return 0 };
    let _ = c.stdin.take().map(|mut i| i.write_all(b));
    c.wait_with_output().map(|o| o.stdout.len() as u64).unwrap_or(0)
}

/// `rgbarkade.v1.Rung`
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Rung {
    #[prost(uint64, tag = "1")]
    pub rate: u64,
    #[prost(uint64, tag = "2")]
    pub fee: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub child: Vec<u8>,
}

/// `rgbarkade.v1.Ladder`
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Ladder {
    #[prost(message, repeated, tag = "1")]
    pub rungs: Vec<Rung>,
}

/// `rgbarkade.v1.WatchtowerRegistration`
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct WatchtowerRegistration {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(message, optional, tag = "2")]
    pub vtxo: Option<OutPoint>,
    #[prost(int64, tag = "3")]
    pub expires_at: i64,
    #[prost(int64, tag = "4")]
    pub threshold_s: i64,
    #[prost(bytes = "vec", repeated, tag = "5")]
    pub branch: Vec<Vec<u8>>,
    #[prost(message, repeated, tag = "6")]
    pub ladders: Vec<Ladder>,
}

pub(crate) fn outpoint_str(o: &OutPoint) -> anyhow::Result<String> {
    use bitcoin::hashes::Hash;
    Ok(format!("{}:{}", bitcoin::Txid::from_slice(&o.txid)?, o.vout))
}

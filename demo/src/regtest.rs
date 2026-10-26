//! Blockchain backend for the arkade-regtest stack (adapted from rust-sdk e2e-tests/tests/common.rs).
use ark_client::error::Error;
use ark_client::{Blockchain, SpendStatus, TxStatus};
use ark_core::ExplorerUtxo;
use base64::Engine;
use bitcoin::hex::FromHex;
use bitcoin::{Address, Amount, OutPoint, Transaction, Txid};
use std::process::Command;


pub struct Regtest {
    esplora: esplora_client::BlockingClient,
    rpc_url: String,
}

impl Regtest {
    pub fn new() -> Self {
        Self {
            esplora: esplora_client::Builder::new("http://localhost:3000/api").build_blocking(),
            rpc_url: "http://localhost:18443".to_string(),
        }
    }

    pub fn rpc(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
        let auth = base64::engine::general_purpose::STANDARD.encode("admin1:123");
        let v = minreq::post(&self.rpc_url)
            .with_header("Content-Type", "application/json")
            .with_header("Authorization", format!("Basic {auth}"))
            .with_body(body)
            .send()?
            .json::<serde_json::Value>()?;
        if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
            anyhow::bail!("bitcoind RPC {method} error: {e}");
        }
        Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    fn rpc_opt(&self, method: &str, params: serde_json::Value) -> Option<serde_json::Value> {
        self.rpc(method, params).ok().filter(|v| !v.is_null())
    }

    pub fn height(&self) -> u64 {
        self.rpc("getblockcount", serde_json::json!([])).unwrap().as_u64().unwrap()
    }

    pub fn raw_tx(&self, txid: &Txid) -> Option<Transaction> {
        let hex = self.rpc_opt("getrawtransaction", serde_json::json!([txid.to_string()]))?;
        let bytes = Vec::from_hex(hex.as_str()?).ok()?;
        bitcoin::consensus::deserialize(&bytes).ok()
    }

    pub fn tx_confirmations(&self, txid: &Txid) -> Option<u64> {
        let v = self.rpc_opt("getrawtransaction", serde_json::json!([txid.to_string(), true]))?;
        // mempool txs have no "confirmations" field: they are NOT mined
        v.get("confirmations").and_then(|c| c.as_u64()).filter(|c| *c > 0)
    }

    fn blocktime(&self, txid: &Txid) -> Option<i64> {
        self.rpc_opt("getrawtransaction", serde_json::json!([txid.to_string(), true]))?
            .get("blocktime")?
            .as_i64()
    }

    fn unspent(&self, txid: &Txid, vout: u32) -> bool {
        match self.rpc("gettxout", serde_json::json!([txid.to_string(), vout, true])) {
            Ok(v) => !v.is_null(),
            Err(_) => true,
        }
    }

    pub fn send_raw(&self, tx: &Transaction) -> anyhow::Result<Txid> {
        let hex = bitcoin::consensus::encode::serialize_hex(tx);
        let r = self.rpc("sendrawtransaction", serde_json::json!([hex, 0, 0.0001]))?;
        Ok(r.as_str().unwrap().parse()?)
    }

    pub fn find_outpoints_blocking(&self, address: &Address) -> Vec<ExplorerUtxo> {
        let spk = address.script_pubkey();
        let txs = match self.esplora.scripthash_txs(&spk, None) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("esplora scripthash_txs failed: {e}");
                return vec![];
            }
        };
        let tip = self.height();
        let mut out = vec![];
        for tx in txs {
            let confs = tx.status.block_height.map(|h| tip.saturating_sub(h as u64) + 1).unwrap_or(0);
            for (i, v) in tx.vout.iter().enumerate() {
                if v.scriptpubkey != spk {
                    continue;
                }
                let outpoint = OutPoint { txid: tx.txid, vout: i as u32 };
                out.push(ExplorerUtxo {
                    outpoint,
                    amount: Amount::from_sat(v.value),
                    confirmation_blocktime: tx.status.block_time,
                    confirmations: confs,
                    is_spent: !self.unspent(&outpoint.txid, outpoint.vout),
                });
            }
        }
        out
    }
}

impl Blockchain for Regtest {
    async fn find_outpoints(&self, address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
        Ok(self.find_outpoints_blocking(address))
    }
    async fn find_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
        Ok(self.raw_tx(txid))
    }
    async fn get_tx_status(&self, txid: &Txid) -> Result<TxStatus, Error> {
        Ok(TxStatus { confirmed_at: self.blocktime(txid) })
    }
    async fn get_output_status(&self, txid: &Txid, vout: u32) -> Result<SpendStatus, Error> {
        let status = self.esplora.get_output_status(txid, vout as u64).map_err(Error::wallet)?;
        Ok(SpendStatus { spend_txid: status.as_ref().and_then(|s| s.txid) })
    }
    async fn broadcast(&self, tx: &Transaction) -> Result<(), Error> {
        self.send_raw(tx).map_err(|e| Error::wallet(e.to_string()))?;
        Ok(())
    }
    async fn get_fee_rate(&self) -> Result<f64, Error> {
        Ok(1.0)
    }
    async fn broadcast_package(&self, txs: &[&Transaction]) -> Result<(), Error> {
        let hexes: Vec<String> = txs.iter().map(|t| bitcoin::consensus::encode::serialize_hex(*t)).collect();
        let r = self
            .rpc("submitpackage", serde_json::json!([hexes, 0.1, 0.0001]))
            .map_err(|e| Error::wallet(e.to_string()))?;
        let msg = r.get("package_msg").and_then(|m| m.as_str()).unwrap_or("");
        if msg != "success" {
            return Err(Error::wallet(format!("submitpackage failed: {r}")));
        }
        Ok(())
    }
}

fn regtest_cmd(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("node")
        .arg(crate::paths::regtest_dir().join("regtest.mjs"))
        .args(args)
        .current_dir(crate::paths::regtest_dir())
        .output()?;
    let s = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        anyhow::bail!("regtest.mjs {:?} failed: {s}", args);
    }
    Ok(s)
}

pub fn faucet(addr: &Address, btc: &str) -> anyhow::Result<()> {
    regtest_cmd(&["faucet", &addr.to_string(), btc, "--confirm"]).map(|_| ())
}

pub fn mine(n: u32) -> anyhow::Result<()> {
    regtest_cmd(&["mine", &n.to_string()]).map(|_| ())
}

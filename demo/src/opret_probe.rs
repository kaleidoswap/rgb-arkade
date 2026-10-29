//! Scenario `opret-probe`: which OP_RETURN outputs does this arkd take in an Arkade tx?
//!
//! No RGB here: Alice sends plain Arkade txs to Bob, each with one extra OP_RETURN output, and the
//! scenario records whether arkd accepts it. The RGB-shaped one is `OP_RETURN <32 bytes>` (the only
//! script an RGB opret commitment can be); the others show where the value limits come from.
use crate::{ark_client, board, now, regtest, step};
use anyhow::anyhow;
use ark_client::GetVtxosRequestFilter;
use ark_core::send::{OffchainTransactions, SendReceiver, build_offchain_transactions};
use bitcoin::{Amount, ScriptBuf, TxOut, psbt};
use regtest::Regtest;
use std::sync::Arc;
use std::time::Duration;

pub async fn run() -> anyhow::Result<()> {
    let state = crate::paths::state().join(format!("run-{}-opret-probe", now()));
    std::fs::create_dir_all(&state)?;
    let regtest = Arc::new(Regtest::new());
    let (alice, _) = ark_client("alice", regtest.clone(), &state).await?;
    let (bob, _) = ark_client("bob", regtest.clone(), &state).await?;
    let info = alice.server_info().await?;
    println!("  >> arkd = {} vtxo_min_amount {:?} dust {}", info.version, info.vtxo_min_amount, info.dust);
    board("alice", &alice, "0.001").await?;

    let push32 = ScriptBuf::new_op_return([0xab; 32]);
    let push33 = ScriptBuf::new_op_return([0xab; 33]);
    let push20 = ScriptBuf::new_op_return([0xab; 20]);
    let cases: Vec<(&str, ScriptBuf, u64)> = vec![
        ("RGB shape OP_RETURN<32B>, 330 sat (dust)", push32.clone(), 330),
        ("RGB shape OP_RETURN<32B>, 1000 sat", push32.clone(), 1000),
        ("RGB shape OP_RETURN<32B>, 329 sat (sub-dust)", push32.clone(), 329),
        ("RGB shape OP_RETURN<32B>, 1 sat", push32.clone(), 1),
        ("RGB shape OP_RETURN<32B>, 0 sat", push32, 0),
        ("OP_RETURN<33B> (not RGB), 330 sat", push33.clone(), 330),
        ("OP_RETURN<33B> (not RGB), 0 sat", push33, 0),
        ("OP_RETURN<20B> (not RGB), 0 sat", push20, 0),
    ];
    let mut results = vec![];
    for (label, script, value) in cases {
        step(label);
        let v = alice
            .list_vtxos_filtered(GetVtxosRequestFilter::Spendable)
            .await?
            .spendable_offchain()
            .map(|x| x.vtxo().clone())
            .max_by_key(|x| x.amount)
            .ok_or_else(|| anyhow!("alice has no VTXO"))?;
        let inputs = alice.resolve_selected_send_inputs(&[v.outpoint]).await?;
        let (bob_addr, _) = bob.get_offchain_address().await?;
        let (chg, chg_v) = alice.get_offchain_address().await?;
        let OffchainTransactions { mut ark_tx, checkpoint_txs } =
            build_offchain_transactions(&[SendReceiver::bitcoin(bob_addr, Amount::from_sat(1_000))], &chg, &inputs, &info)?;
        // the OP_RETURN's value comes out of the change, so the tx stays balanced
        ark_tx.unsigned_tx.output[1].value -= Amount::from_sat(value);
        ark_tx.unsigned_tx.output.insert(0, TxOut { value: Amount::from_sat(value), script_pubkey: script });
        ark_tx.outputs.insert(0, psbt::Output::default());
        let outcome = match alice.submit_built_offchain_send(ark_tx, checkpoint_txs, chg_v.owner_pk()).await {
            Ok(pending) => match alice.sign_and_finalize_pending_tx(pending).await {
                Ok(()) => "ACCEPTED".to_string(),
                Err(e) => format!("submitted, finalize failed: {e:?}"),
            },
            Err(e) => {
                let msg = format!("{e:?}");
                let reason = msg.split("message: \"").nth(1).and_then(|m| m.split('"').next()).unwrap_or(&msg).to_string();
                format!("REJECTED: {reason}")
            }
        };
        println!("  >> {label} => {outcome}");
        results.push(format!("{label} => {outcome}"));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    println!("\nOPRET PROBE on {}:", info.version);
    for r in &results {
        println!("  {r}");
    }
    println!("OPRET PROBE RESULT: DONE");
    Ok(())
}

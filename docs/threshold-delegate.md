# Threshold delegate (closes N3 down to k colluding delegates)

[delegate-renewal.md](delegate-renewal.md) uses one delegate key D in `CLTV(T){D,S}`. S is arkd's signer and cosigns any tx that satisfies the script, so after T **D alone** can move the asset (finding N3). This variant replaces D with **2 of 3 delegates**. Each delegate checks the renewal itself and signs only if the asset stays on a template of the same owner.

- **arkd:** unchanged, master `f863e48` plus `checkpoint-opret` (B) only, image `arkd:rgb-cponly`.
- **Client:** one more rust-sdk hook, `intent::set_rgb_cosign` (see [PATCHES.md](../PATCHES.md)).
- **Code:**
  - `demo/src/delegate.rs`: threshold templates (`Template::threshold`, `subset_leaf`, `delegate_leaves`, `same_parties`) and multi-key hops.
  - `demo/src/cosign.rs`: the co-signer daemon `cosignd` and the coordinator side (`pick_subset`, `cosign_all`).
  - `demo/src/delegated.rs`: the coordinator D1 (the existing delegate daemon, with `cosigners` in the job).
  - `demo/src/delegate_renewal.rs`: the scenario (threshold mode) and the negatives.
- **Run:** `REGTEST_PROFILE=cponly-seconds scripts/regtest-up.sh clean`, then `scripts/run-scenario.sh threshold-delegate [days]` (default 14).

## Templates

A is the owner, S the operator signer, and D1, D2, D3 the delegates. There is one leaf per 2-subset of the delegates, and every leaf contains S:

| Template | Leaves | Used for |
|---|---|---|
| colored | `{A,S}`, `CSV(A)`, `CLTV(T){D1,D2,S}`, `CLTV(T){D1,D3,S}`, `CLTV(T){D2,D3,S}` | Holds the asset. Any 2 delegates can spend it after T |
| spare | `{A,S}`, `CSV(A)`, `{D1,D2,S}`, `{D1,D3,S}`, `{D2,D3,S}` | Plain bitcoin that the delegates refresh in batches |

`Template::threshold(owner, &[D1,D2,D3], k, …)` builds them for any k of n. There are C(n,k) delegate leaves, so 3 for 2-of-3 and 10 for 3-of-5.

## Why arkd accepts this as it is (code, arkd master `f863e48`)

| Question | Answer in arkd | Code |
|---|---|---|
| Which closures does arkd decode? | `CSVMultisig`, `CLTVMultisig`, `Multisig`, `ConditionMultisig`, `ConditionCSVMultisig`. Anything else makes the whole VTXO script invalid | `pkg/ark-lib/script/closure.go` `DecodeClosure` (l.32) |
| Can one leaf be k-of-n? | **No.** `MultisigClosure` is either a chain `k1 CHECKSIGVERIFY … kn CHECKSIG` (all keys) or a `CHECKSIGADD` form whose threshold must equal the number of keys (`len(pubkeys) != AsSmallInt(...)` → not decoded). So a `2-of-3 CHECKSIGADD` leaf is refused | `closure.go` l.105, l.160–180 |
| Can a leaf have more than 2 keys? | Yes. `CLTVMultisigClosure` is `<T> CLTV DROP` plus a `MultisigClosure` with any number of keys | `closure.go` |
| Must every leaf contain S? | Yes, every forfeit (collaborative) closure must contain the signer key, or the VTXO script is refused. The number of leaves is not limited | `pkg/ark-lib/script/vtxo_script.go` `Validate` l.97–133 |
| Does arkd check every key of the spent leaf? | Yes. `verifyTapscriptPartialSigs` needs a valid signature for every key of the leaf. The signer's own key is excused only while arkd has not signed yet (`mustIncludeSignerSig=false`) | `internal/infrastructure/tx-builder/covenantless/builder.go` l.51–230 |
| Where is it called? | Ark tx at submit (`SubmitOffchainTx`, service.go l.1083, before anything is stored), checkpoints at finalize (`FinalizeOffchainTx`, l.1318, with the signer), batch forfeits (l.4620) | `internal/core/application/service.go` |

So "k-of-n" is expressed as **C(n,k) n-of-n leaves**, each `{Di,Dj,S}`. That is the only form arkd accepts without a change. A MuSig2/FROST aggregate key in a `{Dagg,S}` leaf would also pass arkd, but it needs an interactive multi-party signing protocol in every signing step. It was not built.

## One renewal

D1 is the **coordinator**. It holds the stash copy, picks the time and builds every tx, exactly as the single delegate did. D2 and D3 run `cosignd`. Each one has its own key and its own job file (owner base template, asset id, expiry, arkd URL), but no stash.

1. **Pick a subset.** D1 pings the co-signers in order until it has k−1 that are online (`pick_subset`), for example `{D1,D2}` = leaf mask `011`. With fewer than k online it does not renew: "only 1 of 3 delegates online (k = 2)".
2. **Spare to F (batch).** D1 signs the intent proof and the forfeit with its key. The rust-sdk hook `rgb_cosign("intent" | "forfeit")` then sends each PSBT to the chosen co-signers, who add their `tap_script_sigs` (`SIGHASH_DEFAULT`, after the connectors are known).
3. **Hop C to F** through `CLTV(T){Di,Dj,S}`. D1 sends the ark tx, together with the checkpoint and the RGB consignment of the hop, to the co-signers before it submits. After its own checkpoint signature it sends the checkpoint the same way, and then finalizes.
4. **O to the next spare (batch).** Same as step 2.

### What each co-signer checks before it signs (`cosign.rs`, `Cosigner::handle`)

| PSBT | Check |
|---|---|
| intent | Inputs are only the owner's **spare** template (never a colored one). Outputs are only templates of this owner and this delegate set, or OP_RETURN |
| forfeit | The VTXO input is the spare template, and output 0 pays the server's forfeit address |
| ark tx and checkpoint | The checkpoint spends a colored template of this owner through a delegate subset leaf. The ark tx spends only that checkpoint and pays only the spare template, the anchor or OP_RETURN. The consignment is for the job's asset. The checkpoint commits to the shown bundle (`anchor.verify`), and the bundle reveals every transition (opids = known transitions, each `transition.id() == opid`). **Every seal** is an explicit outpoint of a VTXO known to arkd (`vtxo_at`) whose script is a colored template of the **same owner and delegate set** (`same_parties`) |

These checks are the same-owner rule that the receiver-side policy check applies. The difference is that a co-signer applies it **before** the theft is signed, not after.

## Results

Stack: arkd master f863e48 + B only (`arkd:rgb-cponly`), profile `cponly-seconds`. E = 1024 s stands for 7 days, the delegate renews at half life, and the towers fire 150 s before expiry. The baseline is the single-delegate smoke run of the same length (`delegate-renewal 12`, [delegate-renewal.md](delegate-renewal.md)).

Run 1, `threshold-delegate 14` (run-1793634510): **PASS** (setup, Rita's return, report, and Olivia's return; see the note on Olivia below).

Olivia: D2 went offline (for her job) after renewal 2.

| Renewal (day) | Subset | Proof B (gz) | Baseline proof B | PSBTs | Consignment B | Validate ms | Co-signer requests | Waiting for co-signers ms | Renewal s |
|---|---|---|---|---|---|---|---|---|---|
| 1 (3.8) | D1+D2 | 5386 (2854) | 4642 | 5 | 5585 | 247 | 4 | 651 | 4.4 |
| 2 (7.3) | D1+D2 | 7955 (3693) | 6467 | 7 | 6113 | 217 | 4 | 802 | 4.8 |
| 3 (10.8) | D1+D3 | 10524 (4494) | 8292 | 9 | 6641 | 252 | 6 | 878 | 14.3 |
| 4 (14.1) | D1+D3 | 13093 (5283) | 10117 | 11 | 7169 | 246 | 6 | 943 | 13.8 |

- **Latency.** Each renewal spent **0.65–0.94 s** (run 2: 0.66–1.06 s) waiting for co-signers (4–6 signing requests over localhost TCP). Per request, as seen by the co-signer, including its checks: intent 90 ms on average (max 108), forfeit 119 ms (151), ark tx 227 ms (245), checkpoint 255 ms (341). The hop requests are the slowest because the co-signer validates the consignment and asks arkd's indexer about every seal. Wall time per renewal was 4.4–14.3 s, against 3–9 s for the baseline. The 14 s renewals are dominated by batch timing, not by co-signing.
- **Proof size.** **+744 B per renewal** compared with the single delegate (+2569 B instead of +1825 B per renewal, +41 %). The cause is the extra delegate signature (64 B) in both the checkpoint and the ark tx PSBT, plus the larger leaf script and control block (5 leaves, depth 3), all hex in JSON. After 4 renewals the proof is 13.1 kB against 10.1 kB (+29 %). Extrapolated for 60 days (18 renewals): about 5386 + 2569 × 17 ≈ 49 kB, against 35.7 kB. The consignment is unchanged (+528 B per renewal), and validation time is unchanged (about 250 ms).
- **1 delegate offline: renewals continue.** After D2 went offline, D1 picked D3 (`101`) and renewals 3 and 4 ran normally.
- **2 delegates offline: renewals stop and the towers exit.** Rita: D2 and D3 went offline after renewal 1. The coordinator refused 25 times ("only 1 of 3 delegates online (k = 2)"). Tower A fired at 148 s before expiry, and the whole branch was confirmed. Rita validated 1000 units on the exited output (`Unrolled`), the #961 guard refused to re-board it, and she sent it to Gina via CSV (`gina_settled = 1000`).
- **Olivia's return:** discovery took 49 grid addresses and found the current VTXO 1008 s before expiry. The policy check saw 5 witnesses, 4 delegate spends and 0 violations. She sent to Bob through the owner leaf, and Bob received 1000 (proof 15278 B, 5 witnesses, 12 PSBTs, validate 244 ms). The scripted return failed once with a transient Electrum resolver error ("transaction can't be located", for the mined jump-in tx). A manual rerun of the same command a few minutes later passed. The validation step now retries on resolver errors, as the import already did.

Run 2, `threshold-delegate 7` (run-1793637146, 17:32–18:08 CET, same binary plus N3t-c and the validate retry): **PASS** on every step, with no manual step. The days argument only sets when Olivia returns: the script waits for her next renewal after that, and her return came after Rita's, so she again had 4 renewals over 14.1 days. The proof sizes were identical to run 1 (5386 / 7955 / 10524 / 13093 B), with the same subset switch to D1+D3 after D2 went offline. Waiting for co-signers took 661–1063 ms per renewal and wall time was 2.8–14.6 s. Rita: 25 refusals, tower A exited, `Unrolled` 1000, and Gina settled 1000. Bob received 1000 (proof 15278 B, validate+accept 218 ms).

## Negatives

Owner Nora has two colored VTXOs. One is on an honest set {M_D, D2, D3}, where M_D is malicious and is the coordinator holding her stash. The other is on a colluding set {M_D, M_C, D3}. All attempts are after T.

| # | Attempt | Result |
|---|---|---|
| N3t-a | M_D alone signs the subset leaf `{M_D,D2,S}` and submits | **Rejected at submit** by arkd: `INVALID_SIGNATURE (18): invalid signature in ark tx`. Afterwards the VTXO is not spent or locked (`spent false`), and Nora's own `{A,S}` spend of it succeeds |
| N3t-b | M_D asks the honest D2 to co-sign a hop whose ark tx pays M_D | **D2 refuses:** "ark tx output 0 pays a foreign script" (171 ms). Nothing is submitted |
| N3t-c | M_D asks D2 to co-sign a hop that returns the bitcoin to Nora's own spare template (so it passes the output check) but puts the RGB seal on Mallory's VTXO | **D2 refuses on the seal:** "seal …:0 is not a colored template of this owner and delegate set" (280 ms, run 2). Nothing is submitted |
| N3c | M_D and M_C (2 of 3) collude and move the asset to Mallory | **Accepted** by arkd. Mallory's stock RGB check plus the Ark proof: `Ok(1000)`. The receiver policy check flags it (1 violation). This is the residual trust |
| N4 | #961 guard: colored VTXO offered as a boarding input | Refused (kept) |

### Can one delegate lock the VTXO? (griefing)

`SubmitOffchainTx` first checks only that each ark-tx input has **at least one** non-signer signature (service.go l.980–1010). The checkpoints arrive unsigned and are checked at finalize. That looked like a way for one delegate to submit a partially signed tx and leave the VTXO pending. It is not, for two reasons:

- The ark tx spends the checkpoint outputs, whose taptree is `{signer unroll CSV, the same collaborative closure}` (`pkg/ark-lib/offchain/tx.go` l.186–194). Spending them needs **the same subset of keys** as the VTXO leaf.
- Before arkd stores anything, `SubmitOffchainTx` calls `VerifyVtxoTapscriptSigs(arkTx, false)` (l.1083), which needs every key of that leaf except the signer. The VTXO goes into the off-chain tx cache (which makes others get `VTXO_ALREADY_SPENT`) only later (l.1186–1200).

N3t-a confirms this on the stack: rejected at submit with `INVALID_SIGNATURE`, the VTXO still unspent, and the owner's spend afterwards accepted. **One delegate cannot lock it.** k colluding delegates could submit a valid ark tx and withhold the checkpoint signatures, leaving the VTXO pending. But k delegates can steal outright anyway (N3c).

## Residual trust

- **k colluding delegates (here 2 of 3) can move the asset after T** (N3c, measured). The threshold raises the bar from one key to k independent parties, each running the same-owner check. It does not remove the trust. Only receivers that run the policy check reject the result, and the theft leaves public evidence in the consignment.
- **Operator S** is still needed for every renewal. If it stops cosigning, the towers exit (O1), as before.
- **The coordinator (D1) is a single point of liveness and privacy, not of theft.** It holds the stash and builds the txs, so if D1 is offline nobody renews (the towers exit). The co-signers have no stash, so they cannot initiate anything. Any delegate could be a coordinator if it had a stash copy; this was not built. D1 also sees every renewal, as before. The co-signers see one hop's consignment per renewal.
- **Liveness needs k online in the renewal window.** With 1 of 3 offline, renewals continue; with 2 of 3 offline, they stop and the towers exit (both measured).
- **Co-signer checks trust arkd's indexer** for the seal VTXO scripts (`vtxo_at`). A lying indexer could show a foreign VTXO as one of the owner's templates, but it would have to collude with k−1 delegates to matter.
- **Before T** nothing changes: no delegate set can spend the colored VTXO (the CLTV is enforced by arkd, N2).

## Negatives of the design

- C(n,k) leaves: larger taptree, deeper control blocks, larger exits and proofs (+744 B per renewal for 2-of-3). 3-of-5 would need 10 leaves.
- Online coordination: 4–6 signing round trips per renewal inside the batch window. In regtest over localhost this took 0.65–0.94 s per renewal. Over the internet, and with a co-signer in another jurisdiction, add the network RTT ×4–6 plus the seal lookups.
- The rust-sdk hook is process-wide (one renewal at a time per process, enforced by `CosignGuard`).
- Discovery is unchanged (49 addresses for 14 days), because the grid is over T and not over leaves.

## Verified vs not

| Claim | Status |
|---|---|
| arkd accepts `CLTV(T){Di,Dj,S}` and `{Di,Dj,S}` subset leaves; renewals run with 2-of-3 live signing (intent, forfeits, checkpoint, ark tx; `SIGHASH_DEFAULT`) | **Verified** (run 1, 5 renewals) |
| One malicious delegate cannot spend (N3t-a) or lock (griefing) the VTXO | **Verified** on the stack, plus code reading (service.go l.1083, l.1186) |
| An honest co-signer refuses a theft (N3t-b) | **Verified** |
| An honest co-signer refuses a seal-only theft (N3t-c) | **Verified** (run 2) |
| 2 colluding delegates can steal (N3c) | **Verified** (residual trust) |
| 1 offline → renewals continue; 2 offline → towers exit and the owner recovers | **Verified** |
| k-of-n `CHECKSIGADD` leaf refused by arkd | Code reading only (`closure.go` l.173), not run |
| MuSig2/FROST aggregate delegate key | Not built |
| Any delegate as coordinator, coordinator failover | Not built |
| Latency over a real network | Not measured (localhost only) |

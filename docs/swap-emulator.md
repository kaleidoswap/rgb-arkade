# Fixed-price RGB sale enforced by an Arkade-Script covenant (emulator)

Branch `emulator-covenant` (off `swap-noninteractive` 9b6937c). Everything below was run on regtest
in an isolated docker-in-docker stack (arkd `arkd:rgb-cponly` = arkd master f863e48 +
`patches/arkd-checkpoint.patch`, emulator `emulator:rgb` = arkade-os/emulator 7b830f6 (v0.0.9) +
`patches/emulator.patch`) on 2026-11-03, 11:43-12:40 CET, unless marked **unverified**.

## TL;DR

* The maker locks the asset in an **offer VTXO V** and goes offline. Any taker can fill it alone.
  The emulator runs a 122-byte Arkade Script that lets the fill through only if the maker gets at
  least the price and the two RGB commitments the maker signed are the ones on the transactions.
* E2E `swap-emu` **PASS** (run-1793704045-swap-emu). Steps:
  * offer, then four bad fills (all refused by the covenant), then a direct arkd submit (refused);
  * the fill via the emulator;
  * the taker accepts 400 units with arkd paused; the maker gets 30,000 sat and her 600-unit change;
  * cancel refused before its CLTV and accepted after it;
  * renewal of the received outputs into a later batch, batch 1 swept, then a send of the 400 to Carol, who validates offline.
* **Trust: whoever controls the emulator key can spend V alone.** arkd co-signs any input whose
  user-side signatures are valid; it does not run the covenant. In the model the user accepted, the
  operator runs the emulator, so the operator alone can take the offer VTXO (asset and bitcoin).
  This is the same model as Ark Labs' banco.
* **A crash between arkd submit and finalize no longer locks the fill (fixed in the emulator).**
  Without the fix it was measured to lock the offer VTXO and the taker's input (see below). With the
  durable finalize in `patches/emulator.patch`, the emulator writes the signed tx and checkpoints to
  its datadir before calling arkd, and on restart it completes the finalize by itself. E2E
  `swap-emu crash` **PASS** (run-1793705437-swap-emu-crash): the fill was finalized 1.2 s after the
  restart with no resubmit; the maker was paid 30,000 sat; the taker accepted 400 units with arkd
  paused. A crash *before* arkd saw the fill leaves nothing locked: the record is dropped and the
  maker's cancel goes through after the CLTV.
* Code changes: none in arkd. Emulator: +20/-6 lines in `validateCheckpoint` (70-line test) and
  the durable finalize (+369/-3 non-test lines, 260-line test). rgb-lib gets +5/-1 lines (Ark
  proof: accept the ARK extension OP_RETURN) plus a 16-line test.

## Construction

### Offer VTXO V (three leaves, tree built like arkd: `ark_core::Vtxo::new_with_custom_scripts`)

| leaf | script | bytes | who can spend |
|---|---|---|---|
| fill | `<operator> CHECKSIGVERIFY <emulator key tweaked by the covenant> CHECKSIG` | 68 | arkd plus an emulator that ran the covenant |
| cancel | `<H> CLTV DROP <maker> CHECKSIGVERIFY <operator> CHECKSIG` | 73 | maker plus operator after height H |
| exit | `<exit delay> CSV DROP <maker> CHECKSIG` | 37 | maker alone, on-chain (unilateral exit) |

* Tweaked key = `P + tagged_hash("ArkScriptHash", covenant)·G`. This is the emulator's
  `ComputeArkadeScriptPublicKey`.
* The exit leaf is maker-only. arkd's `VtxoScript.Validate` accepts an owner-only CSV exit closure,
  so the maker has a real unilateral exit. That is unlike banco, whose exit leaf includes the
  server key. The exit leaf was **not exercised on-chain** here.

### Offer

1. The maker funds V offchain (2,000 sat) and moves the asset onto V with the on-chain-genesis jump-in.
2. **T1** is colored on V's checkpoint `[OP_RETURN C1, cp, anchor]`: 400 units go to witness
   vout 1 (CP:1), and the 600 change goes to the maker's existing spare VTXO C. **T2** is colored on a
   template ark tx `[OP_RETURN, maker, carrier]`: 400 units go to witness vout 2.
3. The maker signs `sha256(C1 ‖ C2 ‖ V outpoint)` (BIP340). She publishes:
   * the signature, C1 and C2;
   * the covenant, H and V;
   * the T2 consignment (6,179 B) plus its Ark proof (2,746 B);
   * `offer.json` (2,772 B). On branch `protobuf-proof` this is `offer.pb`, 1,815 B (`docs/protobuf.md`).

   Then she goes offline. Building the offer takes 133 ms.

Why a maker signature instead of pinning C1/C2 in the leaf: T1 spends the allocation sealed to V's
outpoint, and V's address commits to the covenant. Pinning C1/C2 would be circular. The signature only
authorizes those exact transitions on that exact VTXO, so it is safe to publish.

* **V outpoint in the message.** An earlier 113-byte version signed only `C1‖C2`. With that version,
  a second VTXO at the same address (same maker, price and H) could be "filled" with the old
  signature: the maker is paid, but the asset on the second VTXO is burned (its checkpoint commits a
  transition for another outpoint).
* **How the covenant reads V.** It takes V from bytes 5..41 of the spent checkpoint, which costs +9 bytes. Unit test
  "signature replayed on another VTXO at the same address" covers this.

### Covenant (`tools/emucov/covenant.go`, 122 B)

The taker supplies the witness, bottom→top: `makerSig, C1, C2, rawCheckpoint`. The emulator reads it
from the ARK extension packet, which is the ark tx's OP_RETURN starting `ARK`, type 0x01, 422 B. The
covenant checks:

1. `HASH256(rawCheckpoint)` equals the txid this input spends, and the vout is 1. Output 0 of
   `rawCheckpoint` is `OP_RETURN C1`. P (the checkpoint's input outpoint) is extracted here.
2. Output 0 of the ark tx is `OP_RETURN C2`.
3. `CHECKSIGFROMSTACK(makerSig, sha256(C1‖C2‖P), maker)`.
4. Output 1 has value ≥ price and scriptPubKey equal to the maker's P2TR script.

Opcodes used: `INSPECTINPUTOUTPOINT`, `PUSHCURRENTINPUTINDEX`, `INSPECTOUTPUTSCRIPTPUBKEY`,
`INSPECTOUTPUTVALUE`, `SUBSTR`, `CAT`, `CHECKSIGFROMSTACK`, `GREATERTHANOREQUAL`, and the alt stack.

Unit tests: 12 engine-level cases (`go test ./tools/emucov`), all PASS:

* valid fill;
* exact price passes, price−1 fails;
* redirected maker output;
* other T2;
* no T2;
* checkpoint with another T1;
* witness C1 mismatch;
* wrong raw checkpoint;
* spending checkpoint vout 0;
* signature by another key;
* swapped C1/C2;
* replay on another VTXO.

### Fill (taker, alone)

1. Build `[B, V(fill leaf, checkpoint opret C1)]`. The outputs are:

   | output | content |
   |---|---|
   | 0 | `OP_RETURN C2` (1 sat) |
   | 1 | maker, 30,000 sat |
   | 2 | taker carrier, 1,000 sat |
   | 3 | taker change |
   | 4 | ARK extension (0 sat) |
   | 5 | anchor |

2. Add the `[0xde]"prevarktx"` PSBT field on every input. The emulator requires it; it carries the tx
   that funds each checkpoint, fetched with `GetVirtualTxs`.
3. Re-anchor the T2 consignment to the final tx and validate it: 400 units on vout 2.
4. Sign B's input and presign the taker's own checkpoint.
5. `POST /v1/tx {arkTx, checkpointTxs}`. The emulator runs the script, signs the V input and V's
   checkpoint, submits to arkd and finalizes. This call took **39 ms**. Build, validation, submit and
   wait for the VTXO took 228 ms in total.

Final ark tx: 710 vB (unsigned).

## Results (run-1793704045-swap-emu, PASS)

### Negatives

Every refusal below left V and B unspent.

| attempt | result |
|---|---|
| underpay by 1 sat | emulator: `OP_VERIFY failed` (price check) |
| other T2 commitment at output 0 | emulator: `OP_EQUALVERIFY failed` |
| checkpoint carrying another T1 | emulator: `OP_EQUALVERIFY failed` |
| maker output redirected to the taker | emulator: `false stack entry at end of script` |
| fill leaf sent straight to arkd, without the emulator | arkd: `INVALID_PSBT_INPUT: missing tapscript spend sig in ark tx input 0` |
| cancel leaf before H | arkd: `FORFEIT_CLOSURE_LOCKED` |

* The emulator's HTTP error is only `500 internal error`. The reason is in its log.
* About the `FORFEIT_CLOSURE_LOCKED` message: arkd's text prints the block *time*, not the height
  (service.go ~914, cosmetic). The metadata carries the height.

### Positive path

* **Fill:** the maker's VTXO is final:1 = 30,000 sat. V is spent by its checkpoint.
* **Taker check:** with arkd paused, the taker validated and accepted **400** on final:2. The proof
  was 8,776 B / 7 PSBTs; the consignment 6,675 B.
* **Maker change:** back online, the maker's 600-unit change on C validates and registers.
* **Cancel:** V2, a bitcoin-only offer, was cancelled with the maker and operator signatures after
  height H.
* **Renewal and spend after renewal:**
  1. A later batch (height 369) gives fresh VTXOs.
  2. Bob's final:2 (400) and Alice's C (600) are renewed by checkpoint hops. Alice's 30,000 sat is
     settled.
  3. Batch 1 expires and is swept at height 630 (V shows swept).
  4. Bob sends the renewed 400 to Carol. Carol validates offline: **400**. Proof 11,751 B /
     10 PSBTs; consignment 7,731 B.

### Crash between arkd submit and finalize

#### Without the fix (run-1793703626-swap-emu-crash, 113-B covenant version)

The test-only image `emulator:rgb-crashtest` exited with code 3 right after arkd `SubmitTx`
succeeded. What was observed:

1. **Taker's HTTP call:** connection error. The emulator exited (3).
2. **arkd state:** V and B are spent by their checkpoints. The fill is pending (Bob's
   `list_pending_offchain_txs` shows it). final:2 does not exist. arkd f863e48 did **not** finalize
   on submit.
3. **Restart and re-submit:** re-submitting the same fill is refused by arkd with
   `INVALID_ARK_PSBT: duplicated offchain tx`.
4. **Taker finalizes:** refused, `INVALID_SIGNATURE in checkpoint tx`. V's checkpoint lacks the
   emulator's signature, which was lost with the process. arkd's `FinalizeOffchainTx` also requires
   arkd's own checkpoint signature, which only the submitter got back.
5. **Maker cancel after H:** `VTXO_ALREADY_SPENT` on 7 tries over 180 s.

So a crash in that window locked both the maker's offer VTXO and the taker's input, and no party
could finish or undo the fill through the normal API. What happens later (exits, fraud reaction,
batch expiry) was **not measured**; with the fix below it no longer matters for this case.

#### The fix: durable finalize (emulator only, no arkd change)

In `internal/application` (`pending.go`, hooks in `service.go`) and `pkg/emulator/pending_proof.go`:

* **Record before submit.** When the emulator is the finalizer, it writes
  `{ark_txid, ark_tx, checkpoints}` (its own and the taker's signatures; no arkd signature yet) to
  `$EMULATOR_DATADIR/pending/<txid>.json` **before** `SubmitOffchainTx`. The write is atomic (temp
  file, fsync, rename, fsync of the directory). If the write fails, the request fails and nothing is
  sent to arkd. `EMULATOR_DATADIR` is `/app/data` in the image; with it empty, behaviour is upstream's.
* **Remove after finalize.** The record is removed once arkd finalized the tx: on submit, or after the
  existing in-memory `retryFinalize`. If that retry gives up, the record stays.
* **Resume loop (the persistent retry; upstream has a TODO for it).** At startup and then every
  10 s, for each record: get arkd's checkpoint signatures with `GetPendingTx`, merge them into the
  recorded checkpoints, call `FinalizeOffchainTx`, remove the record. If arkd holds no pending tx
  with that txid (it never got the tx, or the tx is already finalized), the record is removed and
  nothing is sent.
* **`GetPendingTx` auth.** arkd wants an intent proof (BIP-322 style) over the spent VTXO with a
  `get-pending-tx` message. The emulator builds it for the input it co-signed (the checkpoint's
  input, V) on the same fill leaf, and signs it with its key tweaked by the covenant hash
  (`SignPendingTxProof`). arkd skips its own key when verifying the proof. `SignPendingTxProof`
  refuses any proof whose message is not `get-pending-tx` (tested) and is not exposed through the
  emulator API. Such a proof spends a BIP-322 `to_spend` tx and cannot be broadcast.
* **Resubmit.** If the taker resubmits and arkd answers `duplicated offchain tx`, the emulator runs
  the same completion synchronously and returns success. A per-txid in-flight guard keeps a
  request and the resume loop from working on the same tx at the same time.
* **Unit tests** (`pending_test.go`, fake arkd that keeps pending txs, locks inputs, rejects
  duplicates and verifies the intent proof with `intent.Verify`):
  * record on disk before arkd sees the tx, removed after finalize;
  * retries exhausted, then the resume loop finalizes with arkd's signature;
  * crash after submit, then a new service on the same datadir finalizes;
  * crash after submit, then a duplicated resubmit completes the tx;
  * never accepted, so the record is dropped and nothing is finalized;
  * in-flight guard;
  * no datadir;
  * `SignPendingTxProof` rejects a `delete` intent.

  `go test ./internal/... ./pkg/emulator/...` passes, also with `-race`.

#### With the fix (run-1793705437-swap-emu-crash, 122-B covenant, PASS, 51 s)

The test image now takes `EMULATOR_RGB_TEST_CRASH=before|after` (`patches/emulator-crashtest.patch`,
+8 lines, applied on top of `emulator.patch`). The datadir is the docker volume `emulator-data`
(`scripts/emulator-up.sh`), so it survives replacing the container.

* **N: crash before arkd submit** (bitcoin-only offer V2, same covenant shape):
  1. The taker's call fails. The emulator exited (3) with 1 record on disk. V2 and B are unspent.
  2. `emulator:rgb` is restarted: 0 records (dropped, since arkd held nothing pending). V2 and B are
     still unspent.
  3. The maker's cancel after the CLTV (height 123) is accepted. V2 is spent by the cancel's
     checkpoint.
* **C: crash right after arkd accepted the fill** (the real RGB offer V):
  1. The taker's call fails. The emulator exited (3) with 1 record
     (`601a9abd…json`). V and B are spent by their checkpoints; final:2 does not exist yet.
  2. `emulator:rgb` is restarted on the same datadir and **nobody resubmits**. Bob's final:2 appears
     **1,224 ms** after the restart call (log: `pending tx resolved txid=601a9abd…`). 0 records left.
  3. The rest is the normal path:
     * the maker's output is 30,000 sat;
     * V is spent by the offer checkpoint `41a62b6d…`;
     * Bob validates with arkd paused and accepts **400**;
     * Alice validates and registers her **600** change on C;
     * fill proof 8,776 B / 7 PSBTs.
  4. A later resubmit of the same fill returned success with the same txid and 0 records. Which
     branch answered was not logged (the emulator log was replaced by the next run), so it is
     **unverified** whether arkd said "duplicated" there.

## Trust model

| who | can do | why |
|---|---|---|
| emulator key holder (alone) | **spend V anywhere**: asset and bitcoin | It signs any tx it likes for the fill leaf. arkd co-signs, because it verifies user signatures, not the covenant. |
| operator (arkd) alone, emulator honest and separate | censor (refuse to co-sign); not steal | The fill needs the emulator's signature; cancel and exit need the maker. |
| operator = emulator (accepted assumption, like banco) | **steal V** | Both keys of the fill leaf are held by one party. |
| taker + emulator collusion | steal V | Same as row 1. |
| taker alone | nothing beyond the covenant | Refused by the covenant (negatives above) or by arkd (missing emulator signature). |
| maker | cancel after H (with the operator); exit unilaterally (CSV) | Before H the offer is firm, unless the maker exits on-chain. A race of the maker's exit against an already **finalized** fill falls under ordinary Ark checkpoint and fraud handling; **unverified** here. |

What protects whom:

* **Maker, against any taker:** the covenant (price, destination, exactly T1/T2), as long as the
  emulator is honest.
* **Taker:**
  * The taker signs its own input with SIGHASH_DEFAULT over the whole tx, so nobody can alter the
    taker's side.
  * T1 and T2 are validated offline before signing, from the consignment and Ark proof.
  * The taker's risk is liveness: the emulator or operator refusing. An emulator crash in the
    submit-finalize window is recovered on restart (fixed above), provided its datadir survives.
* **Covenant scope:** it does not constrain the taker's other outputs, the anchor, or the opret
  value. The checkpoint itself is pinned by txid, and arkd rebuilds it.

Compared with the presigned SINGLE|ACP offer (swap-ni):

* There the operator could not take the asset without the maker's signature, but a taker could lock
  V by submitting without finalizing.
* Here a taker cannot lock V on purpose: only the emulator can submit a valid fill, and it finalizes
  in the same call. After an emulator crash in the window, it finalizes on restart. If its datadir
  is lost, the lock is back.
* The trust is much larger (row 1 of the trust-model table).

## Diffs

* **arkd:** none beyond the existing `patches/arkd-checkpoint.patch` (cponly).
* **Emulator, `patches/emulator.patch`** (868 lines):
  * `pkg/emulator/tx.go` +20/−6. `validateCheckpoint` accepts an optional leading 0-value
    `OP_RETURN <32B>` (layout `[opret, cp, anchor]`), so `cpVout = 1`. The ark input must spend that
    vout. Data outputs that carry value, or are not exactly 32 bytes, or extra outputs, are rejected.
  * New `rgb_checkpoint_test.go` (70 lines, 5 subtests).
  * Durable finalize:
    * `internal/application/pending.go` (+269): store, completion via `GetPendingTx`, resume loop;
    * `internal/application/service.go` (+45/−3): record before submit, duplicate handling, remove
      after finalize, `GetPendingTx` in the arkd client interface, datadir and loop in `New`, and
      stopping the loop in `Close`;
    * `pkg/emulator/pending_proof.go` (+54) and `pkg/emulator/service.go` (+1): `SignPendingTxProof`;
    * test: `internal/application/pending_test.go` (+260, 7 subtests + 1).
  * `go test ./internal/... ./pkg/emulator/...` is green, also with `-race`. The repo's `test/`
    integration package needs nigiri and was not run.
* **Emulator, `patches/emulator-crashtest.patch`** (test only, +8 in `internal/application/service.go`,
  on top of `emulator.patch`): `EMULATOR_RGB_TEST_CRASH=before|after` exits right before or after
  arkd `SubmitTx`. `scripts/patches-check.sh` checks it as the stack `emulator.patch+emulator-crashtest.patch`.
* **Images and setup:**
  * `scripts/setup-emulator.sh` builds `emulator:rgb` and `emulator:rgb-crashtest`
    (`emulator-image/Dockerfile`: base ghcr.io/arkade-os/emulator:v0.0.1 + patched static binary)
    and `tools/emucov/emucov`.
  * `scripts/emulator-up.sh` starts the emulator next to the regtest arkd. Its datadir is the
    volume `emulator-data`.
* **rgb-lib, `patches/rgb-lib.patch`:** `opret_layout_ok` accepted exactly one OP_RETURN. It now also
  accepts extra OP_RETURNs whose data starts with `ARK` (the extension). The RGB commitment must still
  be the sub-dust `OP_RETURN<32B>` at output 0. +5/−1, plus unit test
  `opret_layout_allows_only_ark_extension_extra`.
* **Demo:**
  * `demo/src/swap_emu.rs`: scenario `swap-emu [crash]`. `crash` runs N and C above, then the
    taker and maker validation steps.
  * `tools/emucov`: Go helper with commands `script`, `tweak` and `ext`.

## Regressions

After the durable-finalize patch, on a fresh dind stack and a fresh `up-cponly`, 2026-11-03
12:30-12:37 CET. The images were rebuilt with `scripts/setup-emulator.sh`.

| scenario | result | time |
|---|---|---|
| swap-emu crash (run-1793705437) | PASS | 51 s |
| swap-emu (run-1793705488) | PASS | 69 s |
| checkpoint-renew | PASS | 65 s |
| checkpoint-swap | PASS | 42 s |
| checkpoint-offline | PASS | 62 s |
| swap-ni | PASS | 102 s |

In the swap-emu happy path, emulator submit took 39 ms and the fill 242 ms (before the patch: 39 ms
and 228 ms). Covenant, leaves, transaction and proof sizes are unchanged. Before the patch, the
same set had passed at 12:07-12:13 CET. The swap-ni run exercises the changed rgb-lib proof rule on
single-OP_RETURN txs. `scripts/patches-check.sh` reports all 7 entries applying cleanly.

## Unverified / open

* Unilateral exit through V's exit leaf, and the maker-exit-after-fill race: not run.
* What happens without the fix beyond 180 s (expiry, exits): not run. It is moot for this case now.
* The duplicate-resubmit recovery path is covered by unit tests only. In the e2e run the resume loop
  finished first.
* Loss of the emulator datadir (disk loss, or a container without a volume) brings the lock back.
  Not mitigated.
* Recovery depends on arkd keeping the pending tx and answering `GetPendingTx` (verified on arkd
  f863e48 cponly only). arkd versions that finalize on submit, or that expire pending txs, were not
  tested.
* The emulator's `test/` integration suite was not run (needs nigiri).

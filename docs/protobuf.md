# Protobuf instead of JSON for out-of-band payloads

Branch `protobuf-proof` (off `emulator-covenant` 8b345cc). Measured on regtest in the isolated dind
stack on 2026-11-03, 12:48-13:05 CET, unless marked **unverified**.

## What changed

Every out-of-band format this repo defines is protobuf now. arkd's own API is unchanged.

| payload | before | now | schema | Rust type |
|---|---|---|---|---|
| Ark proof (sidecar of a consignment) | `<consignment>.arkproof.json` | `<consignment>.arkproof.pb` | `proto/ark_proof.proto` `ArkProof` | `rgb_lib::ark_proof::pb::ArkProof` |
| Ark fraud proof | `fraud-proof.json` | `fraud-proof.pb` | `ArkFraudProof` | `rgb_lib::ark_proof::pb::ArkFraudProof` |
| swap-ni offer | `offer.json` | `offer.pb` | `proto/offers.proto` `SwapNiOffer` | `demo/src/wire.rs` |
| swap-emu offer | `offer.json` | `offer.pb` | `SwapEmuOffer` | `demo/src/wire.rs` |
| watchtower registration | `registrations/<id>.json` | `registrations/<id>.pb` | `proto/watchtower.proto` `WatchtowerRegistration` | `demo/src/wire.rs` |

Encoding rules:

* Txids are 32 raw bytes in consensus (internal) byte order. Outpoints are `{txid, vout}`.
* PSBTs, transactions, keys, signatures and scripts are raw bytes. There is no base64 or hex on the wire.
* The prost structs are hand-written derives that mirror the `.proto` files. There is no
  build-time codegen and no `protoc` dependency in the Rust build.
* `scripts/proto-check.sh` compiles the `.proto` files with `protoc` (Python `grpcio-tools`) and
  checks every `.pb` file written by a run. Each file must parse with the schema, have no unknown
  fields, and re-serialize to identical bytes. On all `.pb` files of these runs (13:50 CET): 69 ArkProof (including negative-test variants), 4 ArkFraudProof, 1 SwapNiOffer and 2 SwapEmuOffer, with 0 mismatches. No watchtower registration was produced (see below).

Not converted, because they are local state and not exchanged:

* rgb-lib's wallet files `ark_vtxos.json` and `ark_transfers.json`;
* the watchtower's `status/<id>.json`;
* the emulator's `pending/<txid>.json`;
* the demo's `summary.json`, scenario files and web-app API.

Backward compatibility (cheap, so kept): rgb-lib `ArkProof::from_bytes` and
`ArkFraudProof::from_bytes` still read a legacy JSON file, detected by a leading `{`. Writers emit
protobuf only. The offers and the watchtower registration have no JSON reader.

The in-memory `ArkProof` keeps its string fields (txids as hex, PSBTs as base64), so the verifier
code is unchanged. The conversion happens at the file boundary (`to_bytes` / `from_bytes`).

## Size: JSON vs protobuf, same content

The JSON column is the old encoding of the same object, serialized in the same run (the bench
writes it next to the `.pb`; the scenarios compute it in memory). gzip is `gzip -9`.

| payload | JSON B (gzip) | protobuf B (gzip) | raw | gzip |
|---|---|---|---|---|
| swap-emu offer | 2,772 (1,910) | 1,815 (1,261) | -35% | -34% |
| swap-ni offer | 1,938 (1,116) | 1,381 (740) | -29% | -34% |
| swap-emu offer Ark proof (3 PSBTs) | 2,746 (1,558) | 2,010 (1,103) | -27% | -29% |
| swap-ni offer Ark proof (1 PSBT) | 512 (421) | 334 (337) | -35% | -20% |
| swap-emu fill proof (8 PSBTs) | 9,388 (4,213) | 6,931 (2,908) | -26% | -31% |
| swap-ni fill proof (6 PSBTs) | 6,058 (2,750) | 4,433 (2,136) | -27% | -22% |
| 16-hop bench proof (hop 16) | 40,240 (16,307) | 29,610 (11,006) | -26% | -33% |
| swap-emu Carol proof (11 PSBTs) | 12,225 (5,329) | 9,053 (3,722) | -26% | -30% |
| swap-ni Carol proof (9 PSBTs) | 8,895 (3,910) | 6,555 (2,944) | -26% | -25% |

Sources:

* runs `run-1793706673-swap-emu` and `run-1793706580-swap-ni` (cponly);
* bench `state/bench-pb/hops.jsonl` (fast profile, arkd `arkd:rgb`, arkd paused while verifying).

The fill and Carol proofs are measured as shipped, after `add_vtxos`. The build-time figures
compare as follows:

* swap-emu fill proof: 6,523 B / 7 PSBTs, against 8,776 B as JSON in run-1793704045;
* Carol's proof: 8,721 B / 10 PSBTs, against 11,751 B.

Protobuf saves about a quarter on raw proofs: it removes base64 and the JSON keys. Most of the
weight is still the PSBT fields themselves. On the same 16-hop proof, the compact binary format of
the bench is 15,235 B (gzip 9,343), because it keeps only what the verifier reads. Converting the
proof to protobuf does not do that. It would be a separate change: a proof message with the
verifier's fields instead of full PSBTs.

Per hop in the bench, the proof grows by about 2.49 kB as JSON, 1.83 kB as protobuf and 0.94 kB in
compact binary.

## Validation time

Cold verification is a fresh wallet that imports the proof and validates the consignment, with
arkd paused. Each encoding uses its own fresh wallet in the same bench row: protobuf first, then
compact binary, then legacy JSON.

| hop | protobuf | legacy JSON | compact binary | proof import only: pb / JSON |
|---|---|---|---|---|
| 1 | 169 ms | 178 ms | 181 ms | 8.3 / 6.9 ms |
| 4 | 202 ms | 192 ms | 192 ms | 15.9 / 13.4 ms |
| 8 | 247 ms | 248 ms | 262 ms | 26.7 / 26.3 ms |
| 16 | 400 ms | 387 ms | 373 ms | 65.6 / 54.6 ms |

There is no measurable difference: the rows move both ways by up to about 10%, and signature
checks and RGB validation dominate. The import step alone is 2-27% slower for protobuf in 12 of the 16 rows. The likely reason is that `from_bytes` re-encodes the PSBTs to base64 for the unchanged
in-memory type. This is **not** isolated: one run per row, and the order is fixed.

## Tests and runs

* rgb-lib unit test `proof_protobuf_roundtrip_and_legacy_json`: round trip, legacy JSON read, txid
  byte order, raw PSBT bytes, garbage rejected, fraud-proof round trip. `ark_proof::tests`: 3/3 pass.
* demo unit test `registration_protobuf_roundtrip`.
* e2e on cponly, 12:49-12:58 CET, all **PASS**:

  | scenario | time |
  |---|---|
  | swap-ni | 93 s |
  | swap-emu | 199 s |
  | swap-emu crash | 57 s |
  | checkpoint-renew | 48 s |
  | checkpoint-swap | 42 s |
  | checkpoint-offline | 62 s |

  swap-emu's extra time was spent waiting for boarding (6 settle retries), before any proof or
  offer code ran.
* Bench `hops 16 --pause` on the fast profile: 124 s. It does not run on cponly, where the
  stock-lane hop is refused with `AMOUNT_TOO_LOW`; that refusal is unrelated to the format.
* Quick suite (`run-all.sh quick`, fast profile, 13:05-13:28 CET, 1,386 s): 10 of 11 passed on
  the first pass.
  * The failure, `fraud`, never reached proof code. Alice's boarding settle first timed out, then
    arkd refused it with `boarding input ... expired`.
  * A first standalone rerun failed because the worktree lacked the `tools/arkdb` test binary.
  * After copying it from the main checkout, `fraud` **PASS** on a fresh stack (13:41-13:42, 36 s).
    It wrote and verified `fraud-proof.pb` (3,430 B), and the 3 fraud negatives were rejected.
* Passed in the quick suite: basic, basic-onchain-genesis, offchain-transfer, offchain-transfer
  `--offline-proof`, refresh, collab-exit, forfeit-negatives, multi-user, failed-batch,
  multihop-exit.

## Unverified / not done

* The watchtower registration change was not run end-to-end. `long-offline` and
  `exit-watchtower` are time-scaled and not in the quick suite. It is covered by the unit round
  trip only.
* `bench/analyze.py` still fits the `json` keys. The `pb`, `pb_gz` and `pb_xz` keys and
  `cold_json_ms` are new in the rows and are not used by the fit yet.
* gzip sizes of bench files include the file name in the gzip header (a few bytes; `.json` names
  are 5 characters longer).

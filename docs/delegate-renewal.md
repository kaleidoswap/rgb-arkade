# Delegate renewal (O2 + O1)

An owner can stay offline for months while their RGB asset stays on Arkade. A **delegate** key D renews the colored VTXO once every batch period. **Keyless watchtowers** move it on-chain if the operator stops cosigning. This is options O2 and O1 of [operator-risk.md](operator-risk.md).

- **arkd:** master `f863e48` plus `checkpoint-opret` (B) only, image `arkd:rgb-cponly`. No other arkd change is needed.
- **Client:** one more rust-sdk hook, `Client::rgb_settle_inputs` (see [PATCHES.md](../PATCHES.md)).
- **Code:**
  - `demo/src/delegate.rs`: templates, hops, the #961 guard, discovery and the policy check.
  - `demo/src/delegated.rs`: the delegate daemon and a TCP proxy.
  - `demo/src/delegate_renewal.rs`: the scenario.
- **Run:** `REGTEST_PROFILE=cponly-seconds scripts/regtest-up.sh clean`, then `scripts/run-scenario.sh delegate-renewal [days]`.

## Templates

The owner's receive VTXOs use these taproot leaves. A is the owner, D the delegate and S the operator signer.

| Template | Leaves | Used for |
|---|---|---|
| colored | `{A,S}`, `CSV(A)`, `CLTV(T){D,S}` | Holds the asset. D can spend it only after `T`, which is set `W` before the VTXO's expiry |
| spare | `{A,S}`, `CSV(A)`, `{D,S}` | Plain bitcoin that the delegate refreshes in batches. It becomes the next colored VTXO |

- **arkd accepts both templates as they are.** Every collaborative leaf contains S, and `SubmitOffchainTx` enforces CLTV against the tip block time.
- **A leaf without S is refused** for the whole VTXO, so even the owner's `{A,S}` spend is rejected (negative N1).
- **The spare has no CLTV.** rust-sdk builds intent proofs and forfeits with `nLockTime` 0, so a CLTV leaf cannot be spent in a batch.
- **`T` sits on a 64 s grid,** so the owner can find the colored VTXO again (see Discovery).

## One renewal

All three steps are signed by D alone. The owner signs nothing and presigns nothing.

1. **Spare to F.** The delegate puts spare P into the next batch through `{D,S}`. The output F uses the colored template with a new `T = grid(now + E − W)`.
   - The delegate joins the batch live. It signs the intent and the forfeit (`SIGHASH_DEFAULT`, after the connectors are known) with D.
   - rust-sdk's presigned delegate flow (`prepare_delegate_psbts` / `settle_delegate`) cannot be used. It signs forfeits with `ALL|ANYONECANPAY`, and the arkd signer refuses that sighash since v0.9.16 (#58): `unsupported sighash type 0x81`. This was observed on this stack.
2. **Hop C to F** through `CLTV(T){D,S}`.
   - The checkpoint carries the RGB commitment (B, vout 0), with an explicit seal on F, which already exists.
   - Coloring takes two passes: build once to learn the checkpoint, color it, then submit with the same payload. Any failure rolls the transition back (`ark_rollback_transfer`).
   - The ark tx sends C's bitcoin to the spare template, as output O.
3. **O to the next spare.** O still has C's old expiry, so the delegate refreshes it in a batch.

After the three steps, the delegate:

- writes the consignment, the `.arkproof` (carried forward from the previous one, plus F's branch) and `handoff.json`;
- registers F's exit branch with the towers;
- verifies the result with a fresh wallet, recording import time, validation time and the policy check.

There is no seal circularity. F is a batch output that exists before the hop, so the delegate never needs a signature that commits to a VTXO that does not exist yet.

### Why RGB needs no owner signature

RGB itself never asks for a key here:

- The NIA transfer script only checks `svs` (sum of inputs equals sum of outputs).
- Signatures appear only through the `vts` opcode, which the PFA schema uses with the issuer key.

Closing the seal therefore needs only a bitcoin spend of C that commits to the transition, and the CLTV leaf gives D that spend. For a PFA asset the delegate could not renew, because the issuer would have to sign every transition.

## Discovery

The owner keeps A and knows D and S, so they can find the colored VTXO without the delegate:

1. Build the colored template for every `T` on the 64 s grid between going offline and `now + E`.
2. Query the indexer for those addresses (`delegate::discover`).

In the smoke run (14.1 simulated days, 1024 s expiry) the owner scanned 49 addresses. Two simulated months come to about (60 × 146.3 s + 1024 s) / 64 s ≈ 153 addresses; the long run (62.9 days) scanned 160. On a real chain the grid step should be coarser (for example one per hour); the count is then (offline time + E) / step. The scenario checks that the VTXO found this way is the one in the delegate's `handoff.json`. Validation still needs the consignment and proof from the delegate, or from a public DA relay (O7).

## Fallback: watchtowers

- After every renewal the delegate registers the new colored VTXO's exit branch with N towers (`watchtowerd`, keyless). The fee ladders are presigned with the delegate's own fee key.
- If the operator stops cosigning, the renewal fails and the towers broadcast the branch `threshold` seconds before expiry.
- The asset then sits on-chain under the owner's `CSV(A)` leaf. The owner later spends it on-chain with an RGB commitment, and the receiver validates it with the delegate's proof.
- A tower whose VTXO was renewed (spent off-chain) marks its registration `done-refreshed`.

## #961 guard

arkd lets an unrolled VTXO join a batch as a boarding input (#961). For a colored output, the commitment tx would spend the seal with no commitment, and the asset would be gone.

The guard is `delegate::guard_batch_inputs`, and the delegate runs it before every batch. It refuses:

- any outpoint in the job's colored history;
- any outpoint the RGB wallet tracks (`ark_vtxo_status`).

It runs again when the daemon sees the colored VTXO on-chain, and on the owner's side after the exit.

## Policy check (receiver side)

`delegate::policy_check` reads the consignment and the proof. Every witness that spends through a delegate-style leaf (`<T> CLTV DROP <k> CHECKSIGVERIFY <S> CHECKSIG`) must meet two conditions:

- it spends a listed colored template;
- every revealed seal of its transitions sits on a colored template with the same owner and delegate.

Concealed seals and unknown scripts fail closed. **This is a wallet rule, not consensus:** stock RGB validation plus the Ark proof accepts a delegate theft (N3).

## Results

Stack: arkd master f863e48 + checkpoint opret patch B only (`arkd:rgb-cponly`), arkd-wallet v0.9.16, profile `cponly-seconds`: batch expiry E = 1024 s stands for 7 days (1 simulated day ≈ 146.3 s), the delegate renews at half life (`WINDOW_S` 512), towers fire 150 s before expiry. Command: `scripts/run-scenario.sh delegate-renewal <days>`.

Smoke run `delegate-renewal 12` (run-1793616742): **PASS**.

Olivia, owner offline while the delegate renews (4 renewals over 14.1 simulated days):

| Renewal (day) | Proof B (gz) | PSBTs | Consignment B | Import ms | Validate ms | Policy ms |
|---|---|---|---|---|---|---|
| 1 (3.7) | 4642 (2525) | 5 | 5585 | 11 | 178 | 9 |
| 2 (7.1) | 6467 (3141) | 7 | 6113 | 17 | 231 | 18 |
| 3 (10.5) | 8292 (3730) | 9 | 6641 | 19 | 194 | 20 |
| 4 (14.1) | 10117 (4325) | 11 | 7169 | 22 | 197 | 30 |

- Growth per renewal: proof +1825 B (JSON; about +600 B gzipped, 2 PSBTs: the checkpoint and the ark tx of the hop), consignment +528 B (one transition). Verification time stays flat (about 200 ms, dominated by RGB validation).
- Wall time per renewal: 3–9 s (spare batch, hop, leftover refresh).
- Olivia's return: discovery found the current VTXO (49 addresses, 991 s before expiry), the same as in `handoff.json`. The policy check saw 5 witnesses, 4 delegate spends, 0 violations.
- Olivia then sent 1000 to Bob. Bob without the proof: "Invalid consignment". With it: received 1000; proof 11958 B (5086 gz), 12 PSBTs, 5 witnesses; consignment 7697 B (4155 gz); import 20 ms, validate 213 ms.

Rita, operator refusal (the proxy between her delegate and arkd is killed after renewal 1):

- The delegate's next renewal failed (25 refused attempts logged). Towers A and B both reached `done-onchain`. The first tower broadcast F1 146 s before expiry; fee 554 sat (2 sat/vB, child 151 vB) from the presigned ladder.
- The daemon parked the job as `Unrolled`, and the #961 guard refused to offer the on-chain colored output to a batch.
- Rita's return: after CSV she spent the output on-chain to Gina, with the RGB commitment. Gina's plain check (no proof) is rejected; with the proof the policy check saw 1 delegate spend and 0 violations, and Gina settled 1000.
- The asset survived the operator refusal on-chain, and nobody re-boarded the colored output.

Long run `delegate-renewal 60` (run-1793621378, 9279 s wall time, no clock gaps): **PASS**. Setup negatives as in the smoke run.

Olivia, 18 renewals over 62.8 simulated days (every 3.5 days, 3–14 s each):

| Renewal (day) | Proof B (gz) | PSBTs | Consignment B (gz) | Import ms | Validate ms | Policy ms |
|---|---|---|---|---|---|---|
| 1 (3.3) | 4642 (2469) | 5 | 5585 (3067) | 19 | 251 | 16 |
| 4 (13.8) | 10117 (4282) | 11 | 7169 (3885) | 25 | 286 | 50 |
| 8 (27.9) | 17417 (6711) | 19 | 9281 (4959) | 47 | 309 | 98 |
| 12 (41.9) | 24717 (9115) | 27 | 11393 (6026) | 55 | 335 | 172 |
| 16 (55.9) | 32017 (11526) | 35 | 13505 (7090) | 86 | 431 | 376 |
| 18 (62.8) | 35667 (12665) | 39 | 14561 (7617) | 85 | 378 | 393 |

- Renewal 10 completed (hop, proof, leftover refresh) but the delegate's fresh-wallet metrics check failed once with `Stock error` on the on-chain jump-in tx (the same transient indexer lookup seen in the smoke run), so it has no metrics row and its tower registration was skipped; renewal 11 ran normally 8.5 min later. Fixed afterwards in `delegated.rs`: towers are registered before the metrics check, and the check is retried (not exercised by this run).
- Olivia's return at day 62.9: discovery scanned 160 grid addresses and found the current VTXO (1000 s = 6.8 days left), the one in `handoff.json`. Policy check: 19 witnesses, 18 delegate spends, 0 violations (351 ms). Import 105 ms, validate and accept 518 ms.
- Olivia sent 1000 to Bob: plain RGB without the proof gives "Invalid consignment"; with it Bob received 1000. Proof 37508 B (13462 gz), 19 witnesses, 40 PSBTs; consignment 15089 B (7887 gz); import 79 ms, validate and accept 393 ms; policy 0 violations.
- Rita (proxy killed after renewal 1 at 13:20): 26 refused renewal attempts, towers A and B both broadcast the branch at 13:34 (fee 554 sat at 2 sat/vB), the daemon parked the job as on-chain and the guard refused a re-board. Rita spent to Gina via CSV; Gina's first import hit the transient indexer error and succeeded on retry; policy 1 delegate spend, 0 violations; Gina settled 1000.

### Why proof growth is linear, and what 60 real days costs

Each renewal adds one RGB transition whose witness is the hop's checkpoint tx. That checkpoint is never mined: the renewal moves the asset to a new batch, so the old checkpoint is not an ancestor of any later VTXO and no exit ever publishes it. An offline receiver can only accept it with its Ark proof, so every past witness keeps its PSBTs (the checkpoint, about 1.36 kB, and the ark tx, about 0.39 kB) in the proof forever: +2 PSBTs and about +1825 B JSON (about +600 B gzipped) per renewal, plus +528 B of consignment for the transition. The batch trees of the renewals are mined commitments and add nothing. Validation time grows slowly (about 250 → 400 ms); the policy check grows faster (16 → 393 ms) because it re-reads the proof for each witness.

With a real 7-day expiry and renewal at half life (every 3.5 days), 60 days take ⌈60 / 3.5⌉ = 18 renewals, exactly what this run did. Proof for n renewals ≈ 4642 + 1825 × (n − 1) B: 18 renewals ≈ 35.7 kB (12.7 kB gz); one year (105 renewals) ≈ 194 kB (about 65 kB gz); the consignment about 5.6 + 0.53 × n kB.

### Compacting it (client side, not implemented)

The history itself cannot be truncated: RGB validation walks every transition back to genesis, and an off-chain witness stays unmined, so a new receiver always needs evidence for each one. Cheapest options, in order of cost:

1. **Compact encoding:** ship each witness as the final signed checkpoint and ark tx (raw bytes) plus the operator signature, instead of JSON PSBTs with all their fields. Likely about half the size; no protocol change.
2. **Prune for parties that already validated:** an owner or receiver who validated a history stores the result and needs only proofs for the new witnesses (a "proof since witness X" request). This shrinks what the delegate hands back to its owner each renewal, but not what a brand-new receiver needs.
3. **Shorter chains via checkpoints that get mined:** if the hop's checkpoint were published (for example the delegate unrolls the colored VTXO now and then, as the tower exit does), that witness is on-chain and needs no Ark proof. It costs on-chain fees (about 550–650 sat per level) and does not remove the earlier off-chain witnesses, so it only stops growth for witnesses after the anchor if the asset then stays on-chain.
4. **Fewer renewals:** renew later in the life (a smaller W) or a longer batch expiry; each halves the growth rate at the cost of a smaller safety margin.

## Negatives

All run in the setup of every scenario run, with a malicious delegate key "mal" and receivers Nora and Mallory:

| # | Attempt | Result |
|---|---|---|
| N1 | Spend through a `{D}`-only leaf (no S) | Rejected by arkd: `INVALID_VTXO_SCRIPT` "signer pubkey not found". The owner's own spend of such a VTXO is rejected too: arkd checks the whole taptree, so a no-S leaf poisons the VTXO. |
| N2 | Delegate spends via `CLTV(T){D,S}` before T | Rejected: `FORFEIT_CLOSURE_LOCKED`. |
| N3 | Delegate spends via `CLTV(T){D,S}` after T, asset to Mallory's own template | **Accepted by arkd** (it cosigns any valid tx). Mallory's stock RGB check plus the Ark proof: `Ok(1000)`. Only the policy check flags it (1 violation). |
| N4 | Offer a colored on-chain output to a batch (#961) | Refused by the guard ("offered as an on-chain (boarding) input"). |

**Closed down to k colluding delegates by [threshold-delegate.md](threshold-delegate.md)** (2-of-3 subset leaves, no arkd change).

**N3 in plain words:** after T the delegate alone can move the asset and its sats. The leaf needs S, but S is arkd's signer and signs automatically for any tx that satisfies the script. The only thing that catches the theft is the receiver-side policy check, which is a wallet rule and not consensus. A receiver that runs stock RGB validation accepts the stolen asset.

## Residual trust

- **D after T: in practice D alone can move the asset and the sats.** The leaf is `CLTV(T){D,S}`, but S is the operator's signer, which cosigns any tx that satisfies the script (N3). On paper this is "D and S together"; in practice the only extra condition is that the operator is online.
  - Stock RGB plus the Ark proof accepts the theft. The receiver policy check flags it, but only wallets that run it.
  - The theft is public and provable: the consignment shows a delegate-leaf spend to a non-template seal.
- **D before T** cannot use the delegate leaf (N2). The `{A,S}` leaf needs A.
- **S alone** cannot move anything: every leaf needs A or D.
- **After an exit** the on-chain output still has the `CLTV(T){D,S}` leaf. After T the delegate can offer it to a batch as a boarding input (#961), and arkd cosigns, so the exit does not remove D's power; the owner should spend it on-chain before T. The guard only prevents the owner's own mistake.
- **The towers** hold no key. They can only publish the presigned branch: that costs fees, never the asset.

## Mitigations without arkd changes

None is implemented here; the cost is for a production delegate.

| Mitigation | What it changes | Cost |
|---|---|---|
| Threshold delegate: D is a MuSig2/FROST key of k-of-n independent parties, or several leaves `CLTV(T){Di,Dj,S}` | Theft needs k colluding parties. Each party runs the policy check before it signs. arkd sees one key (aggregate) or several leaves (passes `Validate`, every leaf has S). | Live coordination: 3 signing steps per renewal (spare batch, hop, leftover refresh), all inside the batch window, so every party must be online with low latency. More leaves mean more addresses to scan in discovery and larger exit branches. |
| RGB-level "same owner" rule | Receivers reject delegate-leaf spends that leave the colored templates (the policy check here). | As a wallet rule it is cheap (about 10–30 ms per check here) but only social consensus. It cannot be RGB consensus: AluVM and the schema validate state, not bitcoin scripts, so the schema cannot see templates. A custom schema could tag state with an owner id that only the owner's (or issuer's) signature can change; a theft then gives a frozen asset (griefing) instead of a spendable one. NIA or USDt cannot do this, and it requires a signature from someone other than the delegate, which brings back the PFA problem. |
| Short T window W (T close to expiry) | D has power only between T and expiry. An owner back before T makes D powerless. Limits a leaked or stale D key. | Does not stop an active malicious delegate, which only waits for T. W must cover the renewal time + one batch interval + block-time lag + the tower threshold (here 512 s of 1024). A shorter W means more failed renewals that end in an on-chain exit. |
| Bond or slashing | The delegate posts a bond that a proof of theft (public consignment) can claim. | Needs an external contract or a court; capital locked per delegate. |
| O1 only (no delegate) | No theft possible: only towers. | Every period ends on-chain: fees for each asset, and the asset leaves Ark. |
| One delegate per asset | Limits a theft to one asset. | More delegates to run and pay. |
## Privacy cost

The delegate holds a full copy of the stash. It sees:

- the asset, the amount and the complete history (all consignments);
- the owner's templates, and so every VTXO of the asset;
- the renewal timing.

A is fixed across renewals, so all of this links to one owner. Possible improvements:

- derive a fresh owner key per renewal from a key the owner shares as an xpub (discovery would then scan a key range);
- blind the consignment for the delegate (not possible with current RGB: the delegate must validate to build the transition);
- use several delegates, each holding one asset.

## Limits

- The scenario's operator refusal is a TCP proxy that is killed between the delegate and arkd. A real refusal could be selective, but the delegate reacts to any failure the same way.
- Proof size grows by about 1.8 kB (2 PSBTs) per renewal, because checkpoints of other trees are never mined.
- Fees: zero in regtest. A real delegate pays batch fees twice per period (spare and leftover) and pre-funds the tower fee ladders.

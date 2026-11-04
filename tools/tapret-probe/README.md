# tapret-probe

Checks behind [docs/tapret-vs-opret.md](../../docs/tapret-vs-opret.md). Needs `vendor/arkd` (`make setup`).

```bash
cd tools/tapret-probe && go test -v ./...
# cross-check with rgb-consensus (uses the demo's lockfile for pinned deps)
cd rgb-check && cp ../../../demo/Cargo.lock . && cargo run --offline && rm Cargo.lock
```

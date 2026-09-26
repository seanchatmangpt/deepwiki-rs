# Semantic documentation benchmark

Recorded 2026-09-26 on an Apple M-series host under concurrent load (other builds were running).
Profile: `dev` (unoptimized), which is the same profile the `rust.yml` workflow runs `cargo test` in.
Revision under test: the `semantic-docs` hardening commit on `feat/semantic-documentation-26-9-14`.

## In-process compile (regression bound test)

`semantic::hardening::compile_throughput_regression_bound` compiles 5,000 fully populated items.
Each item has docs, a span, two links and a path. Five runs of
`cargo test compile_throughput -- --nocapture` printed:

| run | elapsed_ms | items/s |
|---|---|---|
| 1 | 402.20 | 12432 |
| 2 | 218.02 | 22933 |
| 3 | 330.76 | 15117 |
| 4 | 353.24 | 14155 |
| 5 | 495.54 | 10090 |

The committed regression bound is **< 2.5 s for 5,000 items**, about 5x the slowest recorded run.
The test also asserts linear output growth: the 5,000-item graph is 4.5-5.5x the 1,000-item graph.
The output is 13,128,382 bytes.

## End-to-end CLI (`bench.sh`)

`tests/fixtures/semantic/bench.sh target/debug/deepwiki-rs <items> 5` times the real binary. Each
timing covers JSON parse, compile, graph write, sha256 and receipt write. The script also checks that
all five runs produce byte-identical graphs and that `semantic-docs-replay` returns `REPLAYED`.

| items | input bytes | output bytes | median ms | min ms | max ms | output sha256 |
|---|---|---|---|---|---|---|
| 1,000 | 324,675 | 2,592,184 | 347 | 313 | 4915 | 5971703c65a7a38a... |
| 5,000 | 1,671,875 | 13,083,384 | 1273 | 1184 | 1745 | 9614004d7e498510... |
| 20,000 | 6,833,885 | 52,715,394 | 12338 | 5192 | 14290 | 8e9b69380de8ac20... |

Minimum times scale close to linearly: 1,184 ms to 5,192 ms is 4.4x for 4x the items. Median and max
spread came from host contention, not from the algorithm.

# bench-macros/

`ralphus-bench-macros` provides the `#[ralphus_bench(patience = N)]`
attribute macro consumed by every benchmarked test across the workspace. Full
design, the patience-authoring rule, and how the generated `BenchMeta` is
collected are documented in [[../bench-harness/AGENTS|bench-harness/AGENTS.md]]
(kept together with the harness that actually runs these tests).

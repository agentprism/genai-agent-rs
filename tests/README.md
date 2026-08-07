# Parity and regression tests

This directory ports the non-harness behavioral contract from
`pi/packages/agent/test` case-for-case and adds Rust/genai-specific regression coverage.

## Current checkpoint

- `agent_loop.rs`: 21 mapped cases
- `agent.rs`: 21 mapped cases
- `e2e_scripted.rs`: 10 mapped cases
- Mapped parity: **52/52 green**, zero ignored and zero documented divergences
- M1-M6 completion checkpoint: **112/112 green**
- Current hardened all-feature repository suite: **120/120 green**

Focused targets additionally cover the assistant accumulator, malformed stream handling, event
stream lifetime, runtime configuration, proxy HTTP/SSE and trust-boundary behavior, bounded
streaming JSON/tool arguments, protocol validation, and tool-update settlement/concurrency. All
provider behavior in tests is scripted or served by local mock HTTP endpoints; the live examples
are compiled but never executed by the suite.

```bash
# Verify that the aggregate manifest exactly equals its ordered fragments and pinned TS cases.
python3 scripts/check_test_parity.py

# Compile every library, example, and test target without running providers.
cargo test --all-features --all-targets --no-run

# Run the complete behavioral and regression suite.
cargo test --all-features --all-targets --no-fail-fast
```

`parity_manifest.toml` contains checkpoint metadata plus the aggregate case list. The ordered source
fragments are `parity/agent_loop.toml`, `parity/agent.toml`, and `parity/e2e.toml`. The checker fails
if `expected_cases` drifts, a mapped source/Rust test is missing or duplicated, or any aggregate
case—including its milestone or status—differs from the ordered fragments.

`status = "green"` means the mapped behavior is implemented and passing. `active` is reserved for an
enabled case temporarily red during test-first work; `pending` is allowed only before a substantive
Rust body exists; `divergence` requires a documented and reviewed Rust-specific reason.

Historical checkpoints were **10/52** at the T2 skeleton and **30/52** after the stateless loop.
Those red-baseline phases are complete; M1-M6 reached 112/112, and eight release-hardening
regressions bring the current suite to 120/120.

# Parity and regression tests

This directory ports the non-harness behavioral contract from
`pi/packages/agent/test` case-for-case and adds Rust/genai-specific regression coverage.

## Current checkpoint

- `agent_loop.rs`: 23 mapped cases (of 42 registered tests)
- `agent.rs`: 22 mapped cases (of 31 registered tests)
- `e2e_scripted.rs`: 10 mapped cases (of 10 registered tests)
- `proxy.rs`: 1 mapped case (of 23 registered tests; the rest are proxy regressions)
- Mapped parity: **56/56 green**, zero ignored and zero documented divergences
- M1-M6 completion checkpoint: **112/112 green**
- Current agent-crate all-feature suite: **206/206 green**; the workspace's 7 FFI tests bring
  the agent + FFI gate to **213/213 green**

Focused targets additionally cover the assistant accumulator, malformed stream handling, event
stream lifetime, runtime configuration (including independent session/retry request fields and
shared thinking-budget resolution), fallible tool preparation and before/after hook channels in
sequential and parallel execution, proxy HTTP/SSE and trust-boundary behavior, bounded streaming
JSON/tool arguments, protocol validation, and tool-update settlement/concurrency. The
execution-seam targets (`exec_hooks.rs`, `stream_fn_retry.rs`) pin the request-level
`on_payload`/`on_response` overrides `GenaiStreamFn` honors through the fork's `ExecOptions`
(inherit/replace exactly once per channel per physical attempt, including HTTP errors and
retries, never composing with construction defaults), saturating retry-delay conversion, and
session-id correlation that never enters provider JSON. `fixtures/fresh-consumer/` is the DIST-01
distribution fixture exercised by `scripts/check-distribution.sh`, not by the test suite. All provider
behavior in tests is scripted or served by local mock HTTP endpoints; the live examples are
compiled but never executed by the suite.

```bash
# Verify the pi checkout's git HEAD against upstream_commit, then verify that the aggregate
# manifest exactly equals its ordered fragments and pinned TS cases. PI_ROOT can override the
# default workspace-local `pi/` checkout.
python3 scripts/check_test_parity.py

# Compile every library, example, and test target without running providers.
cargo test --all-features --all-targets --no-run

# Run the complete behavioral and regression suite.
cargo test --all-features --all-targets --no-fail-fast
```

`parity_manifest.toml` contains checkpoint metadata plus the aggregate case list. The ordered
source fragments are `parity/agent_loop.toml`, `parity/agent.toml`, `parity/e2e.toml`, and
`parity/proxy.toml`. The checker fails if the pi checkout is not at `upstream_commit`,
`expected_cases` drifts, a mapped source/Rust test is missing or duplicated, or any aggregate
case—including its milestone or status—differs from the ordered fragments.

`status = "green"` means the mapped behavior is implemented and passing. `active` is reserved for an
enabled case temporarily red during test-first work; `pending` is allowed only before a substantive
Rust body exists; `divergence` requires a documented and reviewed Rust-specific reason.

Historical checkpoints were **10/52** at the T2 skeleton and **30/52** after the stateless loop.
Those red-baseline phases are complete; M1-M6 reached 112/112. Later release-hardening,
production-parity, execution-seam, and latest pi-sync additions bring the mapped matrix to
**56/56**, the current agent crate to **206/206**, and the workspace agent + FFI gate to
**213/213**.

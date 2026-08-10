#!/usr/bin/env bash
# DIST-01 — interim distribution gate for rust-genai-agent + the pinned genai fork.
#
# Until the fork-only `genai` APIs are available from a registry, publication is DISABLED and the
# supported consumption path is: locally packaged crate archives + an exact pinned
# [patch.crates-io] entry in the consumer workspace (see tests/fixtures/fresh-consumer/).
#
# This script is the auditable release gate. It fails on:
#   1. pin drift        — either repo's HEAD moves off its pinned commit without a deliberate,
#                         reviewed update of the pins below;
#   2. version drift    — the genai/rust-genai-agent versions in the two manifests, the
#                         dependency requirement, the fixture, and these pins disagree;
#   3. publish=true     — rust-genai-agent loses its `publish = false` flag (or gains
#                         registry-only metadata), or any CI workflow attempts publication;
#   4. documentation    — docs imply registry-only (crates.io) installation works;
#   5. package contents — `cargo package` output is missing the fork-only API surface;
#   6. fresh consumer   — the packaged archives, extracted into a clean temporary consumer and
#                         patched in place of the registry, fail to build or test.
#
# It performs NO publication action: the only cargo subcommands used are `package --no-verify`,
# `metadata`-free manifest parsing, and `test` inside the temporary consumer. `--no-verify` is
# required because the packaged agent crate's exact `genai` requirement is intentionally not on
# crates.io; the fresh-consumer build below is the real verification.
#
# Env overrides (CI): GENAI_ROOT / AGENT_ROOT point at the two checkouts; CARGO_NET_OFFLINE is
# honored by cargo itself; KEEP_DIST_WORK=1 preserves the temporary consumer for debugging.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
AGENT_ROOT=${AGENT_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}
GENAI_ROOT=${GENAI_ROOT:-$(cd "$AGENT_ROOT/../rust-genai" && pwd)}

# --- Pins: the single source of truth for the interim distribution. ---------------------------
GENAI_PIN_COMMIT="cee6008346595fcf14f77b53ee3bffe682d651c6"
AGENT_PIN_COMMIT="6aaf70d21382e80f6e24c4f420d7fd2cab7cf842"
EXPECTED_GENAI_VERSION="0.7.0-beta.19.1-agentprism"
EXPECTED_AGENT_VERSION="0.2.0"
# Dependency requirement shared by the agent manifest and the fresh-consumer fixture. Cargo's
# [patch] mechanics require the requirement to match at least one published registry version, so
# this is a tight window whose floor is the last published crates.io beta; the exact fork version
# above always arrives via the path dependency locally and the [patch.crates-io] entry in
# packaged consumers. The exactness guarantee is the fork manifest version + the commit pin +
# the fresh-consumer lockfile assertion in gate 7 — not the requirement string.
EXPECTED_GENAI_REQ=">=0.7.0-beta.18, <0.7.0-beta.20"

fail() {
    echo "check-distribution: FAIL: $*" >&2
    exit 1
}

note() {
    echo "check-distribution: $*"
}

# Extract `version` from the [package] section of a manifest.
package_version() {
    awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version = /{gsub(/"/, "", $3); print $3; exit}' "$1"
}

# Extract `publish` from the [package] section of a manifest (empty when absent).
package_publish() {
    awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^publish = /{gsub(/"/, "", $3); print $3; exit}' "$1"
}

# --- Gate 1: commit pin drift -----------------------------------------------------------------
note "gate 1: commit pins"
[ -d "$GENAI_ROOT/.git" ] || fail "genai checkout not found at $GENAI_ROOT"
[ -d "$AGENT_ROOT/.git" ] || fail "rust-genai-agent checkout not found at $AGENT_ROOT"
actual_genai_head=$(git -C "$GENAI_ROOT" rev-parse HEAD)
actual_agent_head=$(git -C "$AGENT_ROOT" rev-parse HEAD)
[ "$actual_genai_head" = "$GENAI_PIN_COMMIT" ] ||
    fail "pin drift: genai HEAD is $actual_genai_head, expected $GENAI_PIN_COMMIT (stage changes stay uncommitted on top of the pin; update the pin only as a deliberate reviewed step)"
[ "$actual_agent_head" = "$AGENT_PIN_COMMIT" ] ||
    fail "pin drift: rust-genai-agent HEAD is $actual_agent_head, expected $AGENT_PIN_COMMIT"
note "gate 1 ok: genai@$GENAI_PIN_COMMIT, rust-genai-agent@$AGENT_PIN_COMMIT"

# --- Gate 2: version drift --------------------------------------------------------------------
note "gate 2: version pins"
genai_version=$(package_version "$GENAI_ROOT/Cargo.toml")
agent_version=$(package_version "$AGENT_ROOT/Cargo.toml")
[ "$genai_version" = "$EXPECTED_GENAI_VERSION" ] ||
    fail "version drift: genai manifest version is '$genai_version', expected '$EXPECTED_GENAI_VERSION'"
[ "$agent_version" = "$EXPECTED_AGENT_VERSION" ] ||
    fail "version drift: rust-genai-agent manifest version is '$agent_version', expected '$EXPECTED_AGENT_VERSION'"
grep -q "genai = { version = \"$EXPECTED_GENAI_REQ\", path = \"../rust-genai\" }" "$AGENT_ROOT/Cargo.toml" ||
    fail "version drift: rust-genai-agent must depend on genai with the pinned requirement window '$EXPECTED_GENAI_REQ' (dual-source path form)"
grep -q "^genai = { path = \"../rust-genai\" }" "$AGENT_ROOT/Cargo.toml" ||
    fail "version drift: rust-genai-agent lost its [patch.crates-io] entry keeping package resolution on the sibling fork"
FIXTURE_MANIFEST="$AGENT_ROOT/tests/fixtures/fresh-consumer/Cargo.toml"
[ -f "$FIXTURE_MANIFEST" ] || fail "missing fresh-consumer fixture manifest at $FIXTURE_MANIFEST"
grep -q "rust-genai-agent-$EXPECTED_AGENT_VERSION" "$FIXTURE_MANIFEST" ||
    fail "version drift: fresh-consumer fixture does not reference rust-genai-agent-$EXPECTED_AGENT_VERSION"
grep -q "genai = \"$EXPECTED_GENAI_REQ\"" "$FIXTURE_MANIFEST" ||
    fail "version drift: fresh-consumer fixture does not use the pinned requirement window '$EXPECTED_GENAI_REQ'"
grep -q "genai-$EXPECTED_GENAI_VERSION" "$FIXTURE_MANIFEST" ||
    fail "version drift: fresh-consumer fixture patch does not reference genai-$EXPECTED_GENAI_VERSION"
note "gate 2 ok: genai $EXPECTED_GENAI_VERSION (req '$EXPECTED_GENAI_REQ'), rust-genai-agent $EXPECTED_AGENT_VERSION"

# --- Gate 3: publication stays disabled ---------------------------------------------------------
note "gate 3: publication disabled"
agent_publish=$(package_publish "$AGENT_ROOT/Cargo.toml")
[ "$agent_publish" = "false" ] ||
    fail "publish gate: rust-genai-agent/Cargo.toml [package] must set 'publish = false' while fork-only genai APIs remain unpublished (found: '${agent_publish:-<absent>}')"
if grep -q "docs.rs/rust-genai-agent" "$AGENT_ROOT/Cargo.toml"; then
    fail "publish gate: rust-genai-agent/Cargo.toml carries registry-only docs.rs metadata while unpublished"
fi
if [ -d "$AGENT_ROOT/.github/workflows" ]; then
    ! grep -rn "cargo publish\|cargo login" "$AGENT_ROOT/.github/workflows" ||
        fail "publish gate: a CI workflow attempts a publication action"
fi
note "gate 3 ok: publish = false, no CI publication action"

# --- Gate 4: documentation must not claim registry-only availability ----------------------------
note "gate 4: documentation honesty"
README="$AGENT_ROOT/README.md"
grep -q "not available on crates.io" "$README" ||
    fail "docs gate: README.md must state the crate is not available on crates.io (interim archive+patch distribution)"
grep -q "check-distribution.sh" "$README" ||
    fail "docs gate: README.md must point consumers at scripts/check-distribution.sh"
! grep -En '^rust-genai-agent = "[0-9]' "$README" ||
    fail "docs gate: README.md shows a registry-only 'rust-genai-agent = \"<version>\"' dependency line, which cannot resolve while unpublished"
! grep -q "resolve the verified crates.io release" "$AGENT_ROOT/docs/architecture.md" ||
    fail "docs gate: docs/architecture.md still claims packaged consumers resolve a crates.io release"
note "gate 4 ok: documentation describes the interim archive+patch flow only"

# --- Gate 5: package both crates locally --------------------------------------------------------
note "gate 5: cargo package (local archives only, no verification build against the registry)"
GENAI_CRATE="$GENAI_ROOT/target/package/genai-$EXPECTED_GENAI_VERSION.crate"
AGENT_CRATE="$AGENT_ROOT/target/package/rust-genai-agent-$EXPECTED_AGENT_VERSION.crate"
rm -f "$GENAI_CRATE" "$AGENT_CRATE"
(cd "$GENAI_ROOT" && cargo package --allow-dirty --no-verify >/dev/null)
[ -f "$GENAI_CRATE" ] ||
    fail "packaging produced no genai-$EXPECTED_GENAI_VERSION.crate (version drift?)"
(cd "$AGENT_ROOT" && cargo package --allow-dirty --no-verify >/dev/null)
[ -f "$AGENT_CRATE" ] ||
    fail "packaging produced no rust-genai-agent-$EXPECTED_AGENT_VERSION.crate (version drift?)"
note "gate 5 ok: $(basename "$GENAI_CRATE"), $(basename "$AGENT_CRATE")"

# --- Gate 6: package contents carry the fork-only API surface -----------------------------------
note "gate 6: package contents"
# (grep the captured listings, not a `tar | grep -q` pipe: grep -q short-circuits on the first
# match and the SIGPIPE would fail the pipeline under `set -o pipefail`.)
genai_listing=$(tar -tzf "$GENAI_CRATE")
agent_listing=$(tar -tzf "$AGENT_CRATE")
grep -q "^genai-$EXPECTED_GENAI_VERSION/src/client/exec_hooks.rs$" <<<"$genai_listing" ||
    fail "genai archive is missing src/client/exec_hooks.rs"
grep -q "^genai-$EXPECTED_GENAI_VERSION/src/client/client_impl.rs$" <<<"$genai_listing" ||
    fail "genai archive is missing src/client/client_impl.rs"
grep -q "pub struct ExecOptions" <(tar -xOf "$GENAI_CRATE" "genai-$EXPECTED_GENAI_VERSION/src/client/exec_hooks.rs") ||
    fail "genai archive exec_hooks.rs lacks the request-level ExecOptions API"
grep -q "^rust-genai-agent-$EXPECTED_AGENT_VERSION/src/stream_fn.rs$" <<<"$agent_listing" ||
    fail "rust-genai-agent archive is missing src/stream_fn.rs"
grep -q "exec_chat_stream_with_exec_options" <(tar -xOf "$AGENT_CRATE" "rust-genai-agent-$EXPECTED_AGENT_VERSION/src/stream_fn.rs") ||
    fail "rust-genai-agent archive stream_fn.rs does not use the request-level exec API"
note "gate 6 ok: archives carry the fork-only exec-hook surface"

# --- Gate 7: fresh consumer from extracted archives ----------------------------------------------
note "gate 7: fresh consumer build+test from extracted archives (no sibling source paths)"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/dist-check.XXXXXX")
cleanup() {
    if [ "${KEEP_DIST_WORK:-0}" = "1" ]; then
        note "KEEP_DIST_WORK=1: preserving $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

mkdir -p "$WORK/vendor" "$WORK/consumer"
tar -xzf "$GENAI_CRATE" -C "$WORK/vendor"
tar -xzf "$AGENT_CRATE" -C "$WORK/vendor"
[ -f "$WORK/vendor/genai-$EXPECTED_GENAI_VERSION/Cargo.toml" ] ||
    fail "extracted genai archive layout drifted"
[ -f "$WORK/vendor/rust-genai-agent-$EXPECTED_AGENT_VERSION/Cargo.toml" ] ||
    fail "extracted rust-genai-agent archive layout drifted"
cp -r "$AGENT_ROOT/tests/fixtures/fresh-consumer/." "$WORK/consumer/"

# The packaged agent manifest must reference the registry requirement window (path stripped),
# which the consumer's [patch.crates-io] then satisfies from the extracted archive.
grep -q "version = \"$EXPECTED_GENAI_REQ\"" "$WORK/vendor/rust-genai-agent-$EXPECTED_AGENT_VERSION/Cargo.toml" ||
    fail "packaged rust-genai-agent manifest lost the pinned genai requirement window"
! grep -q "path = \"../rust-genai\"" "$WORK/vendor/rust-genai-agent-$EXPECTED_AGENT_VERSION/Cargo.toml" ||
    fail "packaged rust-genai-agent manifest still carries the sibling source path"

(cd "$WORK/consumer" && cargo test --quiet)

# The consumer resolution is the exactness proof: genai must resolve to the fork version from
# the extracted archive patch — not to a crates.io release and not to a sibling source path.
resolution=$(cd "$WORK/consumer" && cargo metadata --format-version 1 --quiet | python3 -c '
import json, sys
meta = json.load(sys.stdin)
for package in meta["packages"]:
    if package["name"] == "genai":
        print(package["version"], package["manifest_path"])
        break
')
resolved_version=${resolution%% *}
resolved_path=${resolution#* }
[ "$resolved_version" = "$EXPECTED_GENAI_VERSION" ] ||
    fail "fresh consumer resolved genai $resolved_version, expected the exact fork version $EXPECTED_GENAI_VERSION"
case "$resolved_path" in
    "$WORK/vendor/genai-$EXPECTED_GENAI_VERSION/"*) ;;
    *) fail "fresh consumer resolved genai from '$resolved_path', expected the extracted archive at $WORK/vendor/genai-$EXPECTED_GENAI_VERSION" ;;
esac
note "gate 7 ok: fresh consumer built and tested against the extracted archives (genai pinned at $EXPECTED_GENAI_VERSION via patch)"

note "PASS: interim distribution verified (genai $EXPECTED_GENAI_VERSION @ $GENAI_PIN_COMMIT; rust-genai-agent $EXPECTED_AGENT_VERSION @ $AGENT_PIN_COMMIT). No publication performed."

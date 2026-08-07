#!/usr/bin/env python3
"""Validate the aggregate parity manifest, ordered fragments, and pinned pi-agent cases."""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM_ROOT = ROOT.parent
DEFAULT_MANIFEST = ROOT / "tests" / "parity_manifest.toml"
FRAGMENT_MANIFESTS = (
    ROOT / "tests" / "parity" / "agent_loop.toml",
    ROOT / "tests" / "parity" / "agent.toml",
    ROOT / "tests" / "parity" / "e2e.toml",
)
SOURCE_COUNTS = {
    "pi/packages/agent/test/agent-loop.test.ts": 23,
    "pi/packages/agent/test/agent.test.ts": 22,
    "pi/packages/agent/test/e2e.test.ts": 10,
}
VALID_STATUSES = {"pending", "active", "green", "divergence"}
CASE_RE = re.compile(r"\b(?:it|test)\(\s*[\"'`]([^\"'`]+)[\"'`]")


def fail(message: str) -> None:
    print(f"parity error: {message}", file=sys.stderr)
    raise SystemExit(1)


def upstream_cases() -> set[tuple[str, str]]:
    result: set[tuple[str, str]] = set()
    for source_file, expected_count in SOURCE_COUNTS.items():
        path = UPSTREAM_ROOT / source_file
        if not path.is_file():
            fail(f"upstream source is missing: {path}")
        names = CASE_RE.findall(path.read_text(encoding="utf-8"))
        if len(names) != expected_count:
            fail(
                f"{source_file}: expected {expected_count} concrete cases, found {len(names)}; "
                "update the parity baseline deliberately"
            )
        if len(names) != len(set(names)):
            fail(f"{source_file}: duplicate concrete test names are unsupported")
        result.update((source_file, name) for name in names)
    return result


def mapped_test_exists(rust_test: str) -> bool:
    try:
        relative, fn_name = rust_test.rsplit("::", 1)
    except ValueError:
        return False
    path = ROOT / relative
    if not path.is_file():
        return False
    text = path.read_text(encoding="utf-8")
    pattern = re.compile(rf"\b(?:async\s+)?fn\s+{re.escape(fn_name)}\s*\(")
    return bool(pattern.search(text))


def load_case_list(path: Path, label: str) -> list[dict[str, object]]:
    if not path.is_file():
        fail(f"{label} is missing: {path}")
    with path.open("rb") as handle:
        document = tomllib.load(handle)
    cases = document.get("case")
    if not isinstance(cases, list) or not all(isinstance(case, dict) for case in cases):
        fail(f"{label} must contain only repeated [[case]] tables")
    return cases


def validate_aggregate_fragments(
    aggregate: list[dict[str, object]], expected_cases: int
) -> None:
    fragmented: list[dict[str, object]] = []
    for path in FRAGMENT_MANIFESTS:
        fragmented.extend(load_case_list(path, f"parity fragment {path.name}"))

    if len(fragmented) != expected_cases:
        fail(
            f"ordered parity fragments contain {len(fragmented)} cases, "
            f"but expected_cases is {expected_cases}"
        )
    if aggregate == fragmented:
        return

    for index in range(max(len(aggregate), len(fragmented))):
        aggregate_case = aggregate[index] if index < len(aggregate) else None
        fragment_case = fragmented[index] if index < len(fragmented) else None
        if aggregate_case != fragment_case:
            fail(
                "aggregate manifest does not exactly match its ordered fragments "
                f"at case {index + 1}:\n"
                f"  aggregate: {aggregate_case!r}\n"
                f"  fragment:  {fragment_case!r}"
            )
    fail("aggregate manifest does not exactly match its ordered fragments")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    args = parser.parse_args()

    if not args.manifest.is_file():
        fail(f"manifest is missing: {args.manifest}")
    with args.manifest.open("rb") as handle:
        document = tomllib.load(handle)
    cases = document.get("case")
    if not isinstance(cases, list) or not all(isinstance(case, dict) for case in cases):
        fail("manifest must contain only repeated [[case]] tables")

    expected_cases = document.get("expected_cases")
    source_baseline = sum(SOURCE_COUNTS.values())
    if isinstance(expected_cases, bool) or not isinstance(expected_cases, int):
        fail("manifest expected_cases must be an integer")
    if expected_cases != source_baseline:
        fail(
            f"manifest expected_cases is {expected_cases}, but the pinned source baseline "
            f"is {source_baseline}"
        )
    if len(cases) != expected_cases:
        fail(
            f"aggregate manifest contains {len(cases)} cases, "
            f"but expected_cases is {expected_cases}"
        )
    validate_aggregate_fragments(cases, expected_cases)

    required = {"source_file", "source_name", "rust_test", "milestone", "status"}
    malformed: list[str] = []
    mapped: list[tuple[str, str]] = []
    rust_tests: list[str] = []
    statuses: Counter[str] = Counter()

    for index, case in enumerate(cases, 1):
        if not isinstance(case, dict):
            malformed.append(f"case {index} is not a table")
            continue
        missing = sorted(required - case.keys())
        if missing:
            malformed.append(f"case {index} misses {', '.join(missing)}")
            continue
        source = (str(case["source_file"]), str(case["source_name"]))
        rust_test = str(case["rust_test"])
        status = str(case["status"])
        mapped.append(source)
        rust_tests.append(rust_test)
        statuses[status] += 1
        if status not in VALID_STATUSES:
            malformed.append(f"case {index} has invalid status {status!r}")
        if not str(case["milestone"]).strip():
            malformed.append(f"case {index} has an empty milestone")
        if not mapped_test_exists(rust_test):
            malformed.append(f"case {index} maps to missing Rust test {rust_test!r}")

    if malformed:
        fail("\n  - " + "\n  - ".join(malformed))

    duplicates = [item for item, count in Counter(mapped).items() if count > 1]
    if duplicates:
        fail(f"duplicate upstream mappings: {duplicates!r}")
    rust_duplicates = [item for item, count in Counter(rust_tests).items() if count > 1]
    if rust_duplicates:
        fail(f"duplicate Rust test mappings: {rust_duplicates!r}")

    expected = upstream_cases()
    actual = set(mapped)
    missing = sorted(expected - actual)
    extra = sorted(actual - expected)
    if missing or extra:
        details = []
        if missing:
            details.append("missing:\n    " + "\n    ".join(f"{p}: {n}" for p, n in missing))
        if extra:
            details.append("extra:\n    " + "\n    ".join(f"{p}: {n}" for p, n in extra))
        fail("manifest does not match upstream:\n  " + "\n  ".join(details))

    total = len(expected)
    green = statuses["green"]
    active = green + statuses["active"]
    print(f"parity manifest OK: {total}/{total} cases mapped")
    print(f"green/active: {green}/{active}; green/total: {green}/{total}")
    print("status counts: " + ", ".join(f"{key}={statuses[key]}" for key in sorted(statuses)))


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Dependency-free parity manifest validator for architecture v2 part 2 §10."""

from __future__ import annotations

from collections import Counter, defaultdict
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from typing import Any


VALID_STATUSES = {"semantic-parity", "deliberate-divergence", "planned"}
# "planned" was re-authorized by the owner ruling of 2026-08-26: image generation is in
# scope as Milestone 10, and pi-sync runs may add planned mappings for new upstream tests.
UPSTREAM_TEST_RE = re.compile(r"^packages/(?:ai|agent)/test/.+\.test\.ts$")
LISTED_RUST_TEST_RE = re.compile(r"^(.+): test$")
SNAPSHOT_HEADER_RE = re.compile(
    r"^# Generated from (?P<repository>\S+) (?P<commit>[0-9a-f]{40})\.$"
)
REQUIRED_ALLOWLIST_ROWS = {
    "mutable-partial-provider-signatures",
    "event-stream-only-error-handling",
    "continue-retry-ambiguity",
    "global-default-stream-function",
    "silent-lossy-context-transformations",
    "overloaded-provider-replay-fields",
    "provider-owned-loopback-server",
    "implicit-async-executor-environment",
    "pi-v4-only-session-format",
    "top-level-harness-scaffold",
}
PERMITTED_OWNER_RULINGS = {
    "provider-api-implementations.mdx#not-ported",
}


def nonempty_string(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def load_manifest(path: Path, errors: list[str]) -> dict[str, Any]:
    try:
        with path.open("rb") as manifest_file:
            return tomllib.load(manifest_file)
    except (OSError, tomllib.TOMLDecodeError) as error:
        errors.append(f"cannot read manifest {path}: {error}")
        return {}


def load_snapshot(path: Path, errors: list[str]) -> tuple[str, str, set[str]]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        errors.append(f"cannot read pinned upstream inventory {path}: {error}")
        return "", "", set()

    if not lines:
        errors.append(f"pinned upstream inventory is empty: {path}")
        return "", "", set()

    header = SNAPSHOT_HEADER_RE.fullmatch(lines[0])
    if header is None:
        errors.append(f"invalid pinned upstream inventory header: {lines[0]!r}")
        repository = ""
        commit = ""
    else:
        repository = header.group("repository")
        commit = header.group("commit")

    paths = [line for line in lines if line and not line.startswith("#")]
    invalid = sorted(path for path in paths if UPSTREAM_TEST_RE.fullmatch(path) is None)
    if invalid:
        errors.extend(f"invalid pinned upstream test path: {path}" for path in invalid)
    if paths != sorted(paths):
        errors.append("pinned upstream inventory is not sorted bytewise")
    duplicates = sorted(path for path, count in Counter(paths).items() if count > 1)
    if duplicates:
        errors.extend(f"duplicate pinned upstream test path: {path}" for path in duplicates)

    return repository, commit, set(paths)


def candidate_upstream_checkout(root: Path, errors: list[str]) -> Path | None:
    configured = os.environ.get("PI_UPSTREAM_DIR")
    if configured is not None:
        candidate = Path(configured).expanduser().resolve()
        if not candidate.is_dir():
            errors.append(f"PI_UPSTREAM_DIR is not a directory: {candidate}")
            return None
        return candidate

    candidates = (
        Path("/home/vikash/pi-pin-8fa7eebd2"),
        root.parent / "pi-pin-8fa7eebd2",
    )
    for candidate in candidates:
        if candidate.is_dir():
            return candidate.resolve()
    return None


def live_upstream_inventory(checkout: Path, errors: list[str]) -> tuple[str, set[str]]:
    try:
        revision = subprocess.run(
            ["git", "-C", str(checkout), "rev-parse", "HEAD"],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError) as error:
        errors.append(f"cannot resolve pinned Pi HEAD at {checkout}: {error}")
        revision = ""

    paths: set[str] = set()
    for relative_root in (Path("packages/ai/test"), Path("packages/agent/test")):
        test_root = checkout / relative_root
        if not test_root.is_dir():
            errors.append(f"missing upstream test directory: {test_root}")
            continue
        paths.update(
            path.relative_to(checkout).as_posix()
            for path in test_root.rglob("*.test.ts")
            if path.is_file()
        )
    return revision, paths


def validate_mappings(
    manifest: dict[str, Any], errors: list[str]
) -> tuple[list[dict[str, Any]], dict[str, dict[str, Any]]]:
    raw_mappings = manifest.get("mapping")
    if not isinstance(raw_mappings, list):
        errors.append("manifest must contain at least one [[mapping]] table")
        return [], {}

    mappings: list[dict[str, Any]] = []
    upstream_by_source: dict[str, dict[str, Any]] = {}
    allowlist_occurrences: dict[str, list[int]] = defaultdict(list)

    for index, raw_mapping in enumerate(raw_mappings, start=1):
        label = f"mapping #{index}"
        if not isinstance(raw_mapping, dict):
            errors.append(f"{label} is not a TOML table")
            continue
        mapping = raw_mapping
        mappings.append(mapping)

        source = mapping.get("source")
        status = mapping.get("status")
        rust = mapping.get("rust")
        if not nonempty_string(source):
            errors.append(f"{label} has no non-empty source")
            source = ""
        if status not in VALID_STATUSES:
            errors.append(f"{label} ({source}) has invalid status: {status!r}")
        if not isinstance(rust, list) or any(not nonempty_string(name) for name in rust):
            errors.append(f"{label} ({source}) rust must be an array of non-empty strings")
            rust = []

        if status == "semantic-parity" and not rust:
            errors.append(f"{label} ({source}) semantic-parity requires a Rust test name")
        elif status == "deliberate-divergence":
            if not nonempty_string(mapping.get("reason")):
                errors.append(f"{label} ({source}) deliberate-divergence lacks a reason")
            if not nonempty_string(mapping.get("replacement")):
                errors.append(f"{label} ({source}) deliberate-divergence lacks a replacement")
            allowlist_row = mapping.get("allowlist_row")
            owner_ruling = mapping.get("owner_ruling")
            if allowlist_row in REQUIRED_ALLOWLIST_ROWS and owner_ruling is None:
                allowlist_occurrences[allowlist_row].append(index)
            elif allowlist_row is None and owner_ruling in PERMITTED_OWNER_RULINGS:
                pass
            elif allowlist_row is not None and owner_ruling is not None:
                errors.append(
                    f"{label} ({source}) must cite either a §10.11 allowlist row "
                    "or an owner ruling, not both"
                )
            else:
                errors.append(
                    f"{label} ({source}) is tied to neither a §10.11 allowlist row "
                    f"nor a permitted owner ruling: {allowlist_row!r}, {owner_ruling!r}"
                )

        if isinstance(source, str) and UPSTREAM_TEST_RE.fullmatch(source):
            if source in upstream_by_source:
                errors.append(f"upstream test has duplicate mappings: {source}")
            else:
                upstream_by_source[source] = mapping

    for row in sorted(REQUIRED_ALLOWLIST_ROWS):
        occurrences = allowlist_occurrences.get(row, [])
        if not occurrences:
            errors.append(f"§10.11 allowlist row has no mapping: {row}")

    if manifest.get("planned_test"):
        errors.append("[[planned_test]] entries are forbidden after M9.2")

    return mappings, upstream_by_source


def discover_rust_tests(root: Path, errors: list[str]) -> set[str]:
    cargo = os.environ.get("CARGO", "cargo")
    try:
        result = subprocess.run(
            [cargo, "test", "--workspace", "--", "--list"],
            cwd=root,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
    except OSError as error:
        errors.append(f"cannot run cargo test --workspace -- --list: {error}")
        return set()

    if result.returncode != 0:
        errors.append(
            "cargo test --workspace -- --list failed with exit code "
            f"{result.returncode}:\n{result.stdout.rstrip()}"
        )
        return set()

    return {
        match.group(1)
        for line in result.stdout.splitlines()
        if (match := LISTED_RUST_TEST_RE.fullmatch(line.strip())) is not None
    }


def validate_rust_test_names(
    mappings: list[dict[str, Any]], discovered: set[str], errors: list[str]
) -> None:
    for mapping in mappings:
        if mapping.get("status") == "planned":
            continue
        source = mapping.get("source", "<unknown>")
        rust = mapping.get("rust", [])
        if not isinstance(rust, list):
            continue
        for name in rust:
            if isinstance(name, str) and name not in discovered:
                errors.append(f"mapped Rust test does not exist: {name} (source: {source})")


def main() -> int:
    parity_dir = Path(__file__).resolve().parent
    root = parity_dir.parent
    manifest_path = Path(
        os.environ.get("PARITY_MANIFEST", parity_dir / "manifest.toml")
    ).resolve()
    snapshot_path = parity_dir / "upstream-tests.txt"
    errors: list[str] = []

    manifest = load_manifest(manifest_path, errors)
    snapshot_repository, snapshot_commit, snapshot_tests = load_snapshot(
        snapshot_path, errors
    )
    mappings, upstream_by_source = validate_mappings(manifest, errors)

    manifest_repository = manifest.get("upstream_repository")
    manifest_commit = manifest.get("upstream_commit")
    if manifest_repository != snapshot_repository:
        errors.append(
            "manifest upstream_repository differs from pinned inventory: "
            f"{manifest_repository!r} != {snapshot_repository!r}"
        )
    if manifest_commit != snapshot_commit:
        errors.append(
            "manifest upstream_commit differs from pinned inventory: "
            f"{manifest_commit!r} != {snapshot_commit!r}"
        )

    checkout = candidate_upstream_checkout(root, errors)
    upstream_source = f"snapshot {snapshot_path.relative_to(root)}"
    upstream_tests = snapshot_tests
    if checkout is not None:
        live_commit, live_tests = live_upstream_inventory(checkout, errors)
        upstream_source = f"checkout {checkout}"
        upstream_tests = live_tests
        if live_commit != manifest_commit:
            errors.append(
                "manifest upstream_commit differs from checked-out Pi HEAD: "
                f"{manifest_commit!r} != {live_commit!r}"
            )
        missing_from_snapshot = sorted(live_tests - snapshot_tests)
        stale_in_snapshot = sorted(snapshot_tests - live_tests)
        errors.extend(
            f"pinned inventory is missing checked-out upstream test: {path}"
            for path in missing_from_snapshot
        )
        errors.extend(
            f"pinned inventory contains absent checked-out upstream test: {path}"
            for path in stale_in_snapshot
        )

    missing_mappings = sorted(upstream_tests - upstream_by_source.keys())
    stale_mappings = sorted(upstream_by_source.keys() - upstream_tests)
    errors.extend(f"upstream test is absent from manifest: {path}" for path in missing_mappings)
    errors.extend(f"manifest maps an absent upstream test: {path}" for path in stale_mappings)

    discovered_tests = discover_rust_tests(root, errors)
    validate_rust_test_names(mappings, discovered_tests, errors)

    coverage = Counter(
        upstream_by_source[path].get("status")
        for path in upstream_tests
        if path in upstream_by_source
    )
    manifest_statuses = Counter(mapping.get("status") for mapping in mappings)
    print(f"Parity coverage: {len(upstream_tests)} upstream test files ({upstream_source})")
    for status in ("semantic-parity", "deliberate-divergence", "planned"):
        print(f"  {status}: {coverage[status]}")
    print(f"Manifest mappings: {len(mappings)}")
    for status in ("semantic-parity", "deliberate-divergence", "planned"):
        print(f"  {status}: {manifest_statuses[status]}")
    print(f"§10.11 allowlist rows mapped: {len(REQUIRED_ALLOWLIST_ROWS)}")
    print(f"Unique Rust tests discovered: {len(discovered_tests)}")

    if errors:
        print(f"Parity manifest check failed with {len(errors)} error(s):", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1

    print("Parity manifest check passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

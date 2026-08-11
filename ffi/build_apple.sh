#!/usr/bin/env bash
# build_apple.sh — Build, verify, and package the FFI library for Apple platforms.
#
# Usage:
#   ./build_apple.sh            # dev build: bindings + XCFramework + sync into swiftpm/
#   ./build_apple.sh --clean    # cargo clean first
set -euo pipefail

CRATE="genai_agent_ffi" # [lib] name in ffi/Cargo.toml
ROOT="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT/.." && pwd)"
SWIFTPM_DIR="$ROOT/swiftpm"

# Minimum OS versions for ALL builds (Rust link + C deps like aws-lc-sys via the
# cc crate). Misalignment shows up as cryptic undefined-symbol link errors
# (e.g. __chkstk_darwin). Override via the environment for apps with a lower
# minimum — and lower `platforms:` in swiftpm/Package.swift to match.
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-26.5}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-26.5}"

if [[ "${1:-}" == "--clean" ]]; then
	cargo clean
fi

cd "$WORKSPACE_ROOT"

# All cargo invocations use --locked: fail rather than drift from Cargo.lock.

# -- 1) Debug build for binding generation (needs the .dylib)
cargo build --locked -p genai-agent-ffi

# -- 2) Generate Swift bindings
rm -rf "$ROOT/bindings"
cargo run --locked -p genai-agent-ffi --bin uniffi-bindgen -- generate \
	--library "$WORKSPACE_ROOT/target/debug/lib${CRATE}.dylib" \
	--language swift \
	--out-dir "$ROOT/bindings"

# -- 3) Write the modulemap: must be named module.modulemap for Xcode, and the
#       `link framework` entries make consuming apps auto-link what the Rust
#       code needs (Security/CoreFoundation for TLS; SystemConfiguration for
#       reqwest's system-proxy on macOS) — no manual linker flags in apps.
cat > "$ROOT/bindings/module.modulemap" <<EOF
module ${CRATE}FFI {
	header "${CRATE}FFI.h"
	link framework "Security"
	link framework "CoreFoundation"
	link framework "SystemConfiguration"
	export *
}
EOF

# -- 4) Release staticlibs: iOS device, iOS simulator, macOS (Apple Silicon)
rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin
cargo build --locked -p genai-agent-ffi --release --target=aarch64-apple-ios
cargo build --locked -p genai-agent-ffi --release --target=aarch64-apple-ios-sim
cargo build --locked -p genai-agent-ffi --release --target=aarch64-apple-darwin

# -- 5) Recreate the XCFramework (xcodebuild refuses to overwrite)
rm -rf "$ROOT/apple/GenAIAgent.xcframework"
mkdir -p "$ROOT/apple"
xcodebuild -create-xcframework \
	-library "$WORKSPACE_ROOT/target/aarch64-apple-ios/release/lib${CRATE}.a" -headers "$ROOT/bindings" \
	-library "$WORKSPACE_ROOT/target/aarch64-apple-ios-sim/release/lib${CRATE}.a" -headers "$ROOT/bindings" \
	-library "$WORKSPACE_ROOT/target/aarch64-apple-darwin/release/lib${CRATE}.a" -headers "$ROOT/bindings" \
	-output "$ROOT/apple/GenAIAgent.xcframework"

# -- 6) Verify: every slice present, symbols kept (crash symbolication),
#       deployment target as configured.
verify_slice() {
	local slice="$1" lib="$ROOT/apple/GenAIAgent.xcframework/$1/lib${CRATE}.a"
	[[ -f "$lib" ]] || { echo "  ❌ missing slice: $slice"; exit 1; }
	local arch symbols member members tmp build_version
	arch=$(lipo -info "$lib" | sed 's/.*: //')
	# NOTE: no early-exit readers (`| head`) — they SIGPIPE `ar` under pipefail.
	# Skip non-object members (the first member is the __.SYMDEF index).
	members=$(ar t "$lib")
	member=""
	while IFS= read -r line; do case "$line" in *.o) member=$line; break;; esac; done <<< "$members"
	tmp=$(mktemp -t ffi_verify).o
	ar p "$lib" "$member" > "$tmp"
	# `|| true`: a grep miss in this chain must not abort the build under pipefail.
	build_version=$(otool -l "$tmp" 2>/dev/null | grep -A3 "LC_BUILD_VERSION\|LC_VERSION_MIN" | grep -E "platform|minos|sdk" | awk '{$1=$1};1' | paste -sd' ' -) || true
	[[ -n "$build_version" ]] || build_version="(no build-version load command in spot-checked object)"
	rm -f "$tmp"
	# NOTE: Apple nm errors (exit 1) on objects from Rust's newer LLVM
	# ("Unknown attribute kind") while still listing symbols — tolerate the
	# exit code and validate the count itself.
	symbols=$(nm -gU "$lib" 2>/dev/null | wc -l | tr -d ' ') || true
	[[ "$symbols" =~ ^[0-9]+$ && "$symbols" -gt 0 ]] || { echo "  ❌ symbol check failed for $slice"; exit 1; }
	echo "  ✓ $slice [$arch] symbols=$symbols · $build_version"
}
echo "== Verifying GenAIAgent.xcframework =="
verify_slice ios-arm64
verify_slice ios-arm64-simulator
verify_slice macos-arm64

# -- 7) Sync into the SwiftPM package
rm -rf "$SWIFTPM_DIR/GenAIAgent.xcframework"
cp -R "$ROOT/apple/GenAIAgent.xcframework" "$SWIFTPM_DIR/GenAIAgent.xcframework"
mkdir -p "$SWIFTPM_DIR/Sources/GenAIAgent"
cp "$ROOT/bindings/${CRATE}.swift" "$SWIFTPM_DIR/Sources/GenAIAgent/"

echo "✅ SwiftPM package updated at: $SWIFTPM_DIR"

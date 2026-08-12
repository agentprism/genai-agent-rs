#!/usr/bin/env bash
# release_swift.sh — cut a Swift package release coupled to the rust-genai-agent crate version.
#
#   ffi/release_swift.sh [--dry-run] [--version X.Y.Z]
#
# Pipeline: ffi/build_apple.sh (bindings + 3 release slices + XCFramework + sync) → zip →
# `swift package compute-checksum` → rewrite url/checksum in the root distribution
# Package.swift → validate the manifest.
#
# Without --dry-run it then commits the manifest + the regenerated UniFFI bindings, tags the
# commit with the bare-semver Swift version (`0.2.0` — crate tags are prefixed and ignored by
# the Swift Package Index), pushes main + the tag, and creates the GitHub Release holding the
# XCFramework zip that the root Package.swift references. Release asset URLs are predictable
# (…/releases/download/<tag>/GenAIAgent.xcframework.zip), so the manifest written into the
# tagged commit resolves as soon as the release exists.
#
# Version defaults to the rust-genai-agent crate version (the semantic surface of this stack);
# the Swift package tracks it. Requires: macOS, Xcode, Rust, Swift, and (for the real run) gh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT/.." && pwd)"
DRY_RUN=0
VERSION=""

while [[ $# -gt 0 ]]; do
	case "$1" in
		--dry-run) DRY_RUN=1; shift ;;
		--version) VERSION="$2"; shift 2 ;;
		*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done

# The Swift package version tracks the rust-genai-agent crate version (see root Package.swift).
if [[ -z "$VERSION" ]]; then
	VERSION=$(grep -m1 '^version' "$WORKSPACE_ROOT/agent/Cargo.toml" | cut -d'"' -f2)
fi
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
	echo "❌ Swift package version must be bare semver for SPI (got '$VERSION')" >&2
	exit 1
}
ZIP_NAME="GenAIAgent.xcframework.zip"
ZIP_PATH="$ROOT/apple/$ZIP_NAME"
URL="https://github.com/agentprism/genai-agent-rs/releases/download/$VERSION/$ZIP_NAME"

if [[ "$DRY_RUN" == 0 ]]; then
	git -C "$WORKSPACE_ROOT" diff --quiet && git -C "$WORKSPACE_ROOT" diff --cached --quiet || {
		echo "❌ working tree must be clean for a real release" >&2; exit 1; }
	! git -C "$WORKSPACE_ROOT" rev-parse -q --verify "refs/tags/$VERSION" >/dev/null || {
		echo "❌ tag $VERSION already exists locally" >&2; exit 1; }
	! git ls-remote --tags origin | grep -q "refs/tags/$VERSION$" || {
		echo "❌ tag $VERSION already exists on origin" >&2; exit 1; }
fi

echo "== GenAIAgent $VERSION ($([[ $DRY_RUN == 1 ]] && echo 'dry run' || echo 'RELEASE')) =="

# -- 1) Build bindings + XCFramework and sync into ffi/swiftpm/ (regenerates the
#       UniFFI Swift bindings we commit for consumers).
"$ROOT/build_apple.sh"

# -- 2) Zip the XCFramework (zip root must contain GenAIAgent.xcframework/) and checksum it.
rm -f "$ZIP_PATH"
ditto -c -k --sequesterRsrc --keepParent "$ROOT/apple/GenAIAgent.xcframework" "$ZIP_PATH"
CHECKSUM=$(swift package compute-checksum "$ZIP_PATH")
echo "  ✓ zip: $(du -h "$ZIP_PATH" | cut -f1) · checksum: $CHECKSUM"

# -- 3) Point the root distribution manifest at this release and validate it.
sed -i '' -e "s|releases/download/[^/]*/$ZIP_NAME|releases/download/$VERSION/$ZIP_NAME|" \
	-e "s|checksum: \"[^\"]*\"|checksum: \"$CHECKSUM\"|" "$WORKSPACE_ROOT/Package.swift"
grep -q "checksum: \"$CHECKSUM\"" "$WORKSPACE_ROOT/Package.swift"
(cd "$WORKSPACE_ROOT" && swift package dump-package > /dev/null)
echo "  ✓ root Package.swift updated and valid"

if [[ "$DRY_RUN" == 1 ]]; then
	echo "== dry run: stopping before commit/tag/release. Would publish: =="
	echo "   tag $VERSION → $URL"
	exit 0
fi

# -- 4) Commit (manifest + regenerated committed bindings), tag, push, release.
git -C "$WORKSPACE_ROOT" add Package.swift ffi/swiftpm/Sources/GenAIAgent/
git -C "$WORKSPACE_ROOT" commit -m "release(swift): GenAIAgent $VERSION

XCFramework zip checksum $CHECKSUM; bindings regenerated from ffi crate at this commit."
git -C "$WORKSPACE_ROOT" tag -a "$VERSION" -m "GenAIAgent $VERSION (Swift package; tracks rust-genai-agent $VERSION)"
git -C "$WORKSPACE_ROOT" push origin main
git -C "$WORKSPACE_ROOT" push origin "refs/tags/$VERSION"

gh release create "$VERSION" "$ZIP_PATH" \
	--repo agentprism/genai-agent-rs \
	--verify-tag \
	--title "GenAIAgent $VERSION" \
	--notes "Swift package release tracking rust-genai-agent $VERSION. Consume via SwiftPM: \`.package(url: \"https://github.com/agentprism/genai-agent-rs.git\", from: \"$VERSION\")\`, product \`GenAIAgent\`."

echo "✅ GenAIAgent $VERSION released: $URL"

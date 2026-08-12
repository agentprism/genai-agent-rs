#!/usr/bin/env bash
# publish-crate.sh <crate> <token> <dry-run> — idempotent single-crate publish for publish.yml.
#
# Real publishes skip versions already on the registry (so dispatching the workflow when the
# crates are unchanged is safe and only the Swift stage does work). Dry runs always package +
# verify without uploading.
set -euo pipefail

CRATE="$1"
TOKEN="$2"
DRY_RUN="$3"

VERSION=$(cargo metadata --no-deps --format-version 1 \
	| jq -r ".packages[] | select(.name == \"$CRATE\") | .version")
[[ -n "$VERSION" ]] || { echo "❌ crate $CRATE not found in workspace metadata" >&2; exit 1; }

if [[ "$DRY_RUN" == "true" ]]; then
	cargo publish -p "$CRATE" --token "$TOKEN" --dry-run
	exit 0
fi

STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
	-H "User-Agent: genai-agent-rs-ci (https://github.com/agentprism/genai-agent-rs)" \
	"https://crates.io/api/v1/crates/$CRATE/$VERSION")
if [[ "$STATUS" == "200" ]]; then
	echo "✔ $CRATE $VERSION is already on crates.io — skipping (idempotent publish)"
	exit 0
fi
[[ "$STATUS" == "404" ]] || { echo "❌ unexpected crates.io status $STATUS for $CRATE $VERSION" >&2; exit 1; }

cargo publish -p "$CRATE" --token "$TOKEN"

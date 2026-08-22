#!/usr/bin/env bash
set -euo pipefail

parity_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "${parity_dir}/check.py"

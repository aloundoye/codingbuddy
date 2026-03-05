#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "[coding-quality-benchmark] running deterministic suite..."
cargo test -p codingbuddy-agent --test coding_quality_benchmark -- --nocapture

REPORT_PATH="$ROOT_DIR/.codingbuddy/benchmarks/coding-quality-core.scripted-tool-loop.latest.json"
if [[ -f "$REPORT_PATH" ]]; then
  echo "[coding-quality-benchmark] report: $REPORT_PATH"
else
  echo "[coding-quality-benchmark] warning: report file not found at expected path" >&2
fi

#!/usr/bin/env bash
# Code that runs during a turn must stay cancellable. Blocking HTTP clients
# ignore the turn's CancellationToken, so they are not allowed on that path:
# use codingbuddy_core::runtime (block_on_cancellable / ChatTransport).
# Standalone commands (update, plugin install, mcp oauth/serve) may block,
# because Ctrl-C ends the process.
set -euo pipefail
cd "$(dirname "$0")/.."

turn_path=(
  crates/codingbuddy-core/src
  crates/codingbuddy-llm/src
  crates/codingbuddy-agent/src
  crates/codingbuddy-tools/src/host.rs
  crates/codingbuddy-tools/src/shell.rs
  crates/codingbuddy-mcp/src/sessions.rs
  crates/codingbuddy-hooks/src
)

# Test modules may use blocking clients to build fixtures.
hits=$(grep -rn --include='*.rs' -E 'reqwest::blocking|std::thread::sleep\(retry' "${turn_path[@]}" \
  | grep -vE '/tests?\.rs:|/tests/' || true)

if [ -n "$hits" ]; then
  echo "Blocking HTTP on the turn path (use codingbuddy_core::runtime instead):" >&2
  echo "$hits" >&2
  exit 1
fi
echo "No blocking HTTP on the turn path"

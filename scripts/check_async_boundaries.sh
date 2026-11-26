#!/usr/bin/env bash
# Code that runs during a turn is async end to end and must stay cancellable:
# model requests, tool execution, subprocesses, hooks, LSP and MCP are
# awaited on the shared runtime (codingbuddy_core::runtime), never driven by
# blocking clients, polling threads or nested block_on.
# Standalone commands (update, plugin install, mcp oauth/serve, certification)
# may block, because Ctrl-C ends the process.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
# Print offending lines (test code excluded) and remember the failure.
check() {
  local message=$1 pattern=$2
  shift 2
  local hits
  hits=$(grep -rn --include='*.rs' -E "$pattern" "$@" \
    | grep -vE '/tests?\.rs:|/tests/|// guard: allow' || true)
  if [ -n "$hits" ]; then
    echo "$message" >&2
    echo "$hits" >&2
    fail=1
  fi
}

# Only the code above each file's inline test module counts (a
# `#[cfg(test)]` on a single item or `mod tests;` declaration does not end it).
strip_tests() {
  awk 'FNR==1 {held=""; skip=0}
       skip {next}
       held != "" && /^mod [A-Za-z_]+ *\{/ {skip=1; held=""; next}
       held != "" {print held; held=""}
       /^#\[cfg\(test\)\]/ {held=FILENAME":"FNR":"$0; next}
       {print FILENAME":"FNR":"$0}' "$@"
}

turn_path=(
  crates/codingbuddy-core/src
  crates/codingbuddy-llm/src
  crates/codingbuddy-agent/src
  crates/codingbuddy-tools/src/host.rs
  crates/codingbuddy-tools/src/shell.rs
  crates/codingbuddy-mcp/src/sessions.rs
  crates/codingbuddy-hooks/src
)
check "Blocking HTTP on the turn path (use codingbuddy_core::runtime instead):" \
  'reqwest::blocking|std::thread::sleep\(retry' "${turn_path[@]}"

# The agent awaits model and tool calls. The *_blocking wrappers
# (LlmClientBlockingExt, ToolHostBlockingExt) are for standalone commands
# and tests only.
check "Blocking model/tool calls inside the async agent (await them instead):" \
  'LlmClientBlockingExt|ToolHostBlockingExt|\.(complete(_chat)?(_streaming)?|complete_fim(_streaming)?|list_models|execute(_with_context)?|verify_command)_blocking\(' \
  crates/codingbuddy-agent/src

# Subprocesses on the turn path are tokio children owned by
# execution::AsyncChild (process group / Job object, killed on cancel).
async_process_files=(
  crates/codingbuddy-tools/src/host.rs
  crates/codingbuddy-tools/src/shell.rs
  crates/codingbuddy-tools/src/sandbox.rs
  crates/codingbuddy-mcp/src/sessions.rs
  crates/codingbuddy-lsp/src/client.rs
  crates/codingbuddy-lsp/src/server_manager.rs
  crates/codingbuddy-hooks/src/lib.rs
)
hits=$(strip_tests "${async_process_files[@]}" \
  | grep -E 'OwnedChild|run_owned\(|\.output\(\)|wait_timeout|std::thread::(spawn|Builder|scope|sleep)|recv_timeout|block_on' \
  | grep -v '// guard: allow' || true)
if [ -n "$hits" ]; then
  echo "Blocking process/thread code where async children are required:" >&2
  echo "$hits" >&2
  fail=1
fi

# The model transport awaits requests and stream chunks directly.
hits=$(strip_tests crates/codingbuddy-llm/src/chat_transport.rs crates/codingbuddy-llm/src/streaming.rs \
  crates/codingbuddy-llm/src/lib.rs crates/codingbuddy-llm/src/providers/*.rs \
  | grep -E 'block_on|spawn_blocking|impl (std::io::)?Read for|BufReader::new' \
  | grep -v '// guard: allow' || true)
if [ -n "$hits" ]; then
  echo "Blocking reads or block_on in the async model transport:" >&2
  echo "$hits" >&2
  fail=1
fi

# Turns never call block_on (that would park a runtime worker).
hits=$(strip_tests $(find crates/codingbuddy-agent/src -name '*.rs' -not -path '*/tests/*') \
  | grep -E 'runtime::block_on|block_on_cancellable' \
  | grep -v '// guard: allow' || true)
if [ -n "$hits" ]; then
  echo "block_on inside the async agent:" >&2
  echo "$hits" >&2
  fail=1
fi

# Library code never prints to stderr directly: a stray line is drawn over
# the full-screen UI. Use codingbuddy_core::logging::notice (logged, and
# printed only when no UI owns the terminal) or tracing.
library_crates=$(find crates -maxdepth 1 -type d -name 'codingbuddy-*' \
  ! -name codingbuddy-cli ! -name codingbuddy-testkit ! -name codingbuddy-ui)
for crate in $library_crates; do
  files=$(find "$crate/src" -name '*.rs' -not -path '*/tests/*' -not -name 'tests.rs' \
    -not -name serve.rs -not -name conformance.rs)
  [ -n "$files" ] || continue
  hits=$(strip_tests $files | grep -E 'eprintln!\(' | grep -v '// guard: allow' || true)
  if [ -n "$hits" ]; then
    echo "Library code printing to stderr (use codingbuddy_core::logging::notice):" >&2
    echo "$hits" >&2
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "Turn path is async: no blocking HTTP, processes, threads or block_on; no stray stderr in libraries"

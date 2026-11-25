#!/usr/bin/env bash
# Fails when AGENTS.md lists a crate that is not a workspace member,
# or omits a workspace member. Keeps the agent guide in sync with Cargo.toml.
set -euo pipefail
cd "$(dirname "$0")/.."

members=$(sed -n '/^members *= *\[/,/\]/p' Cargo.toml | grep -oE 'crates/codingbuddy-[a-z-]+' | sed 's#crates/##' | sort -u)
documented=$(grep -oE '^- `codingbuddy-[a-z-]+`' AGENTS.md | sed -E 's/^- `(.*)`/\1/' | sort -u)

status=0
for c in $documented; do
  if ! grep -qx "$c" <<<"$members"; then
    echo "AGENTS.md lists \`$c\`, which is not a workspace member" >&2
    status=1
  fi
done
for c in $members; do
  if ! grep -qx "$c" <<<"$documented"; then
    echo "Workspace member \`$c\` is missing from the AGENTS.md crate list" >&2
    status=1
  fi
done

count=$(wc -l <<<"$members" | tr -d ' ')
if ! grep -q "^${count}-crate Rust workspace" AGENTS.md; then
  echo "AGENTS.md crate count is wrong; expected '${count}-crate Rust workspace'" >&2
  status=1
fi

[ "$status" -eq 0 ] && echo "AGENTS.md crate list matches Cargo.toml ($count crates)"
exit "$status"

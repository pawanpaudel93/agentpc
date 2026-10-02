#!/bin/sh
# Every check CI runs, in one place, so a release runs exactly the same ones:
#   scripts/check.sh
# Needs cargo, shellcheck, python3, bash and dash (dash is skipped with a note if missing),
# and the `claude` CLI for the plugin validation.
set -eu
cd "$(dirname "$0")/.."
say() { printf '\033[1m==>\033[0m %s\n' "$*"; }

say "cargo fmt --check"
cargo fmt --check
say "cargo clippy"
cargo clippy --locked --all-targets -- -D warnings
say "cargo test"
cargo test --locked

say "shell syntax"
sh -n install.sh
bash -n guests/arch/build.sh
for f in guests/arch/prepare.sh guests/arch/x86apps.sh guests/ubuntu/*.sh guests/helpers/*; do
  sh -n "$f"
done
say "shellcheck"
shellcheck -S warning install.sh guests/arch/*.sh guests/ubuntu/*.sh guests/helpers/* \
  scripts/check.sh scripts/release.sh scripts/test-guest-helpers.sh

say "guest helpers (bash)"
HELPER_SH=bash bash scripts/test-guest-helpers.sh >/dev/null
if command -v dash >/dev/null 2>&1; then
  say "guest helpers (dash)"
  HELPER_SH=dash dash scripts/test-guest-helpers.sh >/dev/null
else
  echo "    (no dash here; CI's guest-helpers job runs them with it)"
fi

say "smoke.py compiles"
python3 -m py_compile scripts/smoke.py

say "plugin manifests parse"
find plugin .claude-plugin -name '*.json' -print0 | xargs -0 -n1 python3 -m json.tool >/dev/null

# AGENTS.md's tool table is the agent contract; the plugin skill and the server must have
# every tool it lists.
say "AGENTS.md, SKILL.md and the MCP server list the same tools"
# Every `tool` (or `tool(args)`) in the first column of the table.
tools=$(sed -n '/^## Using the VMs/,/^## /p' AGENTS.md | grep '^| `' | cut -d'|' -f2 |
  grep -oE '`[a-z_]+[`(]' | tr -d '`(' | sort -u)
[ -n "$tools" ] || { echo "no tools found in AGENTS.md" >&2; exit 1; }
missing=
for t in $tools; do
  grep -qE "\`${t}[\`(]" plugin/skills/agentpc/SKILL.md || missing="$missing $t(SKILL.md)"
  grep -qE "fn $t\(|name = \"$t\"" src/mcp.rs || missing="$missing $t(src/mcp.rs)"
done
[ -z "$missing" ] || { echo "missing:$missing" >&2; exit 1; }

say "claude plugin validate"
claude plugin validate plugin --strict

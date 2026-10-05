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
  scripts/check.sh scripts/release.sh scripts/test-guest-helpers.sh \
  scripts/test-arch-power-key.sh

say "guest helpers (bash)"
HELPER_SH=bash bash scripts/test-guest-helpers.sh >/dev/null
if command -v dash >/dev/null 2>&1; then
  say "guest helpers (dash)"
  HELPER_SH=dash dash scripts/test-guest-helpers.sh >/dev/null
else
  echo "    (no dash here; CI's guest-helpers job runs them with it)"
fi

say "Arch power key reaches logind"
sh scripts/test-arch-power-key.sh >/dev/null

say "static site SEO and links"
python3 scripts/test-site.py

say "smoke.py prints no viewer link or credential"
python3 scripts/test-smoke-logging.py

# The concurrent client behind smoke.py's recovery checks (a fake server), the recovery helpers
# (canned replies), and smoke.py's VM-free protocol checks against the freshly built debug binary.
# The real server runs in a fresh, empty AGENTPC_HOME (it sees no VM at all) with
# AGENTPC_KEEP_RUNNING=1, over a ghost VM name: no VM is created, started or stopped.
# `scripts/test-smoke-client.py --fake-only` runs without starting any agentpc server.
say "smoke.py concurrent client and VM-free protocol checks"
cargo build --locked --quiet
python3 scripts/test-smoke-client.py

say "VM test scripts compile"
python3 -m py_compile scripts/smoke.py scripts/lifecycle-stress.py scripts/test-smoke-logging.py \
  scripts/test-smoke-client.py

say "plugin manifests parse"
find plugin .claude-plugin -name '*.json' -print0 | xargs -0 -n1 python3 -m json.tool >/dev/null

# AGENTS.md's tool table is the agent contract; the plugin skill, the README and the server
# must have every tool it lists (the site's list and count are checked by test-site.py).
say "AGENTS.md, SKILL.md, README.md and the MCP server list the same tools"
# Every `tool` (or `tool(args)`) in the first column of the table.
tools=$(sed -n '/^## Using the VMs/,/^## /p' AGENTS.md | grep '^| `' | cut -d'|' -f2 |
  grep -oE '`[a-z_]+[`(]' | tr -d '`(' | sort -u)
[ -n "$tools" ] || { echo "no tools found in AGENTS.md" >&2; exit 1; }
missing=
for t in $tools; do
  grep -qE "\`${t}[\`(]" plugin/skills/agentpc/SKILL.md || missing="$missing $t(SKILL.md)"
  grep -qE "\`${t}[\`(]" README.md || missing="$missing $t(README.md)"
  grep -qE "fn $t\(|name = \"$t\"" src/mcp.rs || missing="$missing $t(src/mcp.rs)"
done
[ -z "$missing" ] || { echo "missing:$missing" >&2; exit 1; }

say "claude plugin validate"
claude plugin validate plugin --strict

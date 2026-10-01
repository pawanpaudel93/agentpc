#!/bin/sh
# Cut an agentpc release from this Mac: scripts/release.sh X.Y.Z [--dry-run] [-y]
#
# Bumps the version, runs the CI checks, builds dist/ (binary tarball, MCP bundle,
# .sha256 files, filled-in server.json), then commits, tags, pushes main and the tag,
# and creates the GitHub Release with those assets. Uses your existing `gh` login.
#
#   --dry-run  build dist/ and show what would be published; commit, tag, push nothing
#   -y         don't ask before publishing
#
# RELEASE_ALLOW_DIRTY=1 lets --dry-run run on a dirty tree (for testing the script only).
set -eu

REPO="pawanpaudel93/agentpc"
TARGET="aarch64-apple-darwin"
# Files that record the version (manifest.json's is filled at pack time instead).
VERSION_FILES="Cargo.toml Cargo.lock server.json plugin/.claude-plugin/plugin.json .claude-plugin/marketplace.json
site/index.html site/cli.html site/guide.html site/mcp.html"

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() {
  printf '\033[31merror:\033[0m %s\n' "$*" >&2
  exit 1
}
has() { command -v "$1" >/dev/null 2>&1; }

usage() {
  echo "usage: scripts/release.sh X.Y.Z [--dry-run] [-y]" >&2
  exit 2
}

VERSION=""
DRY_RUN=0
YES=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -y | --yes) YES=1 ;;
    -h | --help) usage ;;
    -*) die "unknown option: $arg" ;;
    *)
      [ -z "$VERSION" ] || usage
      VERSION="${arg#v}"
      ;;
  esac
done
[ -n "$VERSION" ] || usage
printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' ||
  die "version must look like X.Y.Z (got $VERSION)."
TAG="v$VERSION"

cd "$(dirname "$0")/.."
ROOT=$(pwd)
DIST="$ROOT/dist"

# --- preconditions ---------------------------------------------------------
[ "$(uname -s)" = "Darwin" ] && [ "$(uname -m)" = "arm64" ] ||
  die "releases are built on an Apple Silicon Mac (native arm64 shell)."
for tool in git cargo jq node npx shasum tar codesign gh; do
  has "$tool" || die "$tool is required."
done
gh auth status >/dev/null 2>&1 || die "gh is not logged in; run: gh auth login"

[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || die "not on main."
if [ -n "$(git status --porcelain)" ]; then
  if [ "$DRY_RUN" = 1 ] && [ "${RELEASE_ALLOW_DIRTY:-0}" = 1 ]; then
    say "Working tree is dirty; continuing because RELEASE_ALLOW_DIRTY=1"
    # shellcheck disable=SC2086 # word-split on purpose
    [ -z "$(git status --porcelain -- $VERSION_FILES)" ] ||
      die "the version files have uncommitted changes."
  else
    die "working tree is not clean; commit or stash first."
  fi
fi

say "Fetching origin"
git fetch --quiet origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
  die "main is not in sync with origin/main; pull or push first."
# CI also runs what this Mac can't (the dash job); publish only a commit it passed.
if [ "$DRY_RUN" = 0 ]; then
  ci=$(gh run list --repo "$REPO" --workflow ci.yml --commit "$(git rev-parse HEAD)" \
    --json status,conclusion --jq '.[0] | "\(.status) \(.conclusion)"' 2>/dev/null || true)
  case "$ci" in
    "completed success") ;;
    "") die "CI hasn't run on $(git rev-parse --short HEAD) yet; push main and wait for it." ;;
    completed*) die "CI failed on $(git rev-parse --short HEAD) ($ci); fix it first." ;;
    *) die "CI is still running on $(git rev-parse --short HEAD); wait for it to pass." ;;
  esac
fi
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  die "tag $TAG already exists locally."
fi
if git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null 2>&1; then
  die "tag $TAG already exists on origin."
fi

CURRENT=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')

# Until the release commit exists, put the version files back on any exit.
BUNDLE=$(mktemp -d "${TMPDIR:-/tmp}/agentpc-release.XXXXXX")
RESTORE=1
cleanup() {
  rm -rf "$BUNDLE"
  # shellcheck disable=SC2086
  [ "$RESTORE" = 0 ] || git checkout --quiet -- $VERSION_FILES
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# --- version ---------------------------------------------------------------
# Releasing the version already in Cargo.toml (e.g. the first release) skips the bump.
if [ "$CURRENT" = "$VERSION" ]; then
  say "Version files already at $VERSION"
else
  say "Setting version $CURRENT -> $VERSION"
fi
OLD_RE=$(printf '%s' "$CURRENT" | sed 's/\./\\./g')
# subst FILE SED_EXPR: edit FILE in place (portable across BSD/GNU sed).
subst() {
  sed "$2" "$1" >"$1.tmp" && mv "$1.tmp" "$1"
}
subst Cargo.toml "1,/^version = /s/^version = \"$OLD_RE\"/version = \"$VERSION\"/"
subst plugin/.claude-plugin/plugin.json "s/\"version\": \"$OLD_RE\"/\"version\": \"$VERSION\"/"
subst .claude-plugin/marketplace.json "s/\"version\": \"$OLD_RE\"/\"version\": \"$VERSION\"/"
subst server.json "s/\"version\": \"$OLD_RE\"/\"version\": \"$VERSION\"/; s#/v$OLD_RE/agentpc-$OLD_RE\.mcpb#/$TAG/agentpc-$VERSION.mcpb#"
# The site's nav badge: any old version (prereleases too), so a missed bump can't stick.
SITE_FILES="site/index.html site/cli.html site/guide.html site/mcp.html"
for f in $SITE_FILES; do
  subst "$f" "s#<span class=\"version\" translate=\"no\">v[0-9A-Za-z.+-]*</span>#<span class=\"version\" translate=\"no\">$TAG</span>#"
  grep -qF "<span class=\"version\" translate=\"no\">$TAG</span>" "$f" || die "$f: nav version badge not updated to $TAG."
done
cargo update --quiet --offline --workspace

[ "$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')" = "$VERSION" ] ||
  die "Cargo.toml did not take the new version."
[ "$(jq -r .version plugin/.claude-plugin/plugin.json)" = "$VERSION" ] || die "plugin.json not updated."
[ "$(jq -r '.plugins[0].version' .claude-plugin/marketplace.json)" = "$VERSION" ] || die "marketplace.json not updated."
[ "$(jq -r .version server.json)" = "$VERSION" ] || die "server.json not updated."
case "$(jq -r '.packages[0].identifier' server.json)" in
  */"$TAG/agentpc-$VERSION.mcpb") ;;
  *) die "server.json identifier not updated." ;;
esac

# --- checks (the same script CI runs) ----------------------------------------
scripts/check.sh

# --- build -----------------------------------------------------------------
say "Building $TARGET"
cargo build --release --locked --target "$TARGET"
BIN="target/$TARGET/release/agentpc"
# Stripping can drop the linker's ad-hoc signature; arm64 macOS refuses unsigned binaries.
codesign --force --sign - "$BIN"
codesign --verify "$BIN" || die "codesign verification failed for $BIN"
GOT=$("$BIN" --version)
[ "$GOT" = "agentpc $VERSION" ] || die "built binary reports '$GOT', expected 'agentpc $VERSION'."

# --- dist/ -----------------------------------------------------------------
rm -rf "$DIST"
mkdir -p "$DIST"
# No AppleDouble (._*) files in the archives.
export COPYFILE_DISABLE=1

NAME="agentpc-$VERSION-$TARGET"
say "Packaging $NAME.tar.gz"
mkdir -p "$DIST/$NAME"
cp "$BIN" LICENSE README.md "$DIST/$NAME/"
tar -C "$DIST" -czf "$DIST/$NAME.tar.gz" "$NAME"
rm -rf "${DIST:?}/$NAME"
(cd "$DIST" && shasum -a 256 "$NAME.tar.gz" >"$NAME.tar.gz.sha256")

MCPB="agentpc-$VERSION.mcpb"
say "Packaging $MCPB"
mkdir -p "$BUNDLE/server"
jq --arg v "$VERSION" '.version = $v' packaging/mcpb/manifest.json >"$BUNDLE/manifest.json"
cp "$BIN" "$BUNDLE/server/agentpc"
cp LICENSE README.md "$BUNDLE/"
npx -y @anthropic-ai/mcpb@2.1.2 validate "$BUNDLE/manifest.json"
npx -y @anthropic-ai/mcpb@2.1.2 pack "$BUNDLE" "$DIST/$MCPB"
(cd "$DIST" && shasum -a 256 "$MCPB" >"$MCPB.sha256")

say "Filling dist/server.json"
MCPB_SHA=$(awk '{print $1}' "$DIST/$MCPB.sha256")
jq --arg v "$VERSION" --arg url "https://github.com/$REPO/releases/download/$TAG/$MCPB" --arg sha "$MCPB_SHA" \
  '.version = $v | .packages[0].identifier = $url | .packages[0].fileSha256 = $sha' \
  server.json >"$DIST/server.json"

ASSETS="dist/$NAME.tar.gz dist/$NAME.tar.gz.sha256 dist/$MCPB dist/$MCPB.sha256 dist/server.json"

# --- release notes ----------------------------------------------------------
# gh's --generate-notes lists merged PRs, and this repo commits straight to main, so build
# the notes from the Conventional Commit subjects since the previous tag instead.
say "Writing dist/NOTES.md"
PREV=$(git describe --tags --abbrev=0 2>/dev/null || true)
RANGE=${PREV:+$PREV..}HEAD
section() { # section TITLE REGEX: one bullet per matching subject, prefix stripped
  lines=$(git log --no-merges --format=%s "$RANGE" | grep -E "$2" | grep -v '^chore: release' |
    sed -E 's/^[a-z]+(\([^)]*\))?!?: /- /') || true
  [ -z "$lines" ] || printf '### %s\n\n%s\n\n' "$1" "$lines"
}
{
  printf 'Install or upgrade:\n\n```sh\ncurl -fsSL https://agentpc.pawanpaudel.com.np/install.sh | sh\n```\n\n'
  section "Features" '^feat(\(|!|:)'
  section "Fixes" '^fix(\(|!|:)'
  section "Performance" '^perf(\(|!|:)'
  section "Docs" '^docs(\(|!|:)'
  section "Other" '^(refactor|build|ci|test|chore)(\(|!|:)'
  [ -z "$PREV" ] || printf '**Full changelog**: https://github.com/%s/compare/%s...%s\n' "$REPO" "$PREV" "$TAG"
} >"$DIST/NOTES.md"

# --- publish ---------------------------------------------------------------
cat <<EOF

Ready to publish $TAG:
  commit  "chore: release $TAG"  ($VERSION_FILES; skipped if unchanged)
  tag     $TAG (annotated)
  push    git push origin main && git push origin $TAG
  release gh release create $TAG --repo $REPO --notes-file dist/NOTES.md
          $ASSETS
EOF

if [ "$DRY_RUN" = 1 ]; then
  echo
  # shellcheck disable=SC2086
  git --no-pager diff --stat -- $VERSION_FILES
  say "Dry run: nothing committed, tagged, pushed or released; version edits reverted. Assets are in dist/."
  exit 0
fi

if [ "$YES" != 1 ]; then
  printf '\nPublish %s? [y/N] ' "$TAG"
  read -r answer || answer=""
  case "$answer" in
    y | Y | yes | YES) ;;
    *) die "aborted; version edits reverted." ;;
  esac
fi

say "Committing and tagging $TAG"
# shellcheck disable=SC2086
git add -- $VERSION_FILES
git diff --cached --quiet || git commit --quiet -m "chore: release $TAG"
RESTORE=0
git tag -a "$TAG" -m "$TAG"

say "Pushing main and $TAG"
git push origin main || die "push of main failed; the commit and tag $TAG are local only."
git push origin "$TAG" || die "push of $TAG failed; retry: git push origin $TAG"

say "Creating GitHub Release $TAG"
# shellcheck disable=SC2086
gh release create "$TAG" --repo "$REPO" --verify-tag --title "$TAG" --notes-file "$DIST/NOTES.md" $ASSETS ||
  die "release failed; retry: gh release create $TAG --repo $REPO --verify-tag --title $TAG --notes-file dist/NOTES.md $ASSETS"

cat <<EOF

Released $TAG: https://github.com/$REPO/releases/tag/$TAG

Follow-ups:
  MCP Registry: mcp-publisher login github && mcp-publisher publish dist/server.json
                (brew install mcp-publisher)
  Linux images (separate): oras login ghcr.io, then for each of ubuntu, ubuntu-x86apps, arch and arch-x86apps:
                           agentpc image build <image> && agentpc image push <image>
EOF

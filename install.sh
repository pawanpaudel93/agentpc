#!/bin/sh
# agentpc installer: curl -fsSL https://raw.githubusercontent.com/pawanpaudel93/agentpc/main/install.sh | sh
#
# Environment:
#   AGENTPC_VERSION      version to install, e.g. 0.1.0 (default: latest release)
#   AGENTPC_INSTALL_DIR  where the binary goes (default: $HOME/.local/bin)
#   AGENTPC_NO_MCP=1     skip registering the MCP server with installed agents
#
# Re-running upgrades in place.
set -eu

REPO="pawanpaudel93/agentpc"
TARGET="aarch64-apple-darwin"
INSTALL_DIR="${AGENTPC_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() {
  printf '\033[31merror:\033[0m %s\n' "$*" >&2
  exit 1
}
has() { command -v "$1" >/dev/null 2>&1; }

# --- platform --------------------------------------------------------------
[ "$(uname -s)" = "Darwin" ] || die "agentpc runs on macOS only (found $(uname -s))."
# A Rosetta shell reports x86_64 on Apple Silicon, so ask the kernel as well.
if [ "$(uname -m)" != "arm64" ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" != "1" ]; then
  die "agentpc needs an Apple Silicon Mac (M1 or later); this Mac is $(uname -m)."
fi
has curl || die "curl is required."
has shasum || die "shasum is required."
has tar || die "tar is required."

# --- version ---------------------------------------------------------------
if [ -n "${AGENTPC_VERSION:-}" ]; then
  VERSION="${AGENTPC_VERSION#v}"
else
  say "Looking up the latest release"
  TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1) || TAG=""
  if [ -z "$TAG" ]; then
    # API rate-limited or unreachable: follow the releases/latest redirect instead.
    TAG=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null |
      sed -n 's#.*/releases/tag/##p') || TAG=""
  fi
  [ -n "$TAG" ] || die "could not determine the latest release; set AGENTPC_VERSION=x.y.z and retry."
  VERSION="${TAG#v}"
fi

ASSET="agentpc-$VERSION-$TARGET.tar.gz"
URL="https://github.com/$REPO/releases/download/v$VERSION/$ASSET"

# --- download + verify -----------------------------------------------------
TMP=$(mktemp -d "${TMPDIR:-/tmp}/agentpc-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM

say "Downloading agentpc $VERSION"
curl -fSL --retry 3 --progress-bar -o "$TMP/$ASSET" "$URL" || die "download failed: $URL"
curl -fsSL --retry 3 -o "$TMP/$ASSET.sha256" "$URL.sha256" || die "download failed: $URL.sha256"

say "Verifying checksum"
EXPECTED=$(awk '{print $1; exit}' "$TMP/$ASSET.sha256")
ACTUAL=$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')
[ -n "$EXPECTED" ] && [ "$EXPECTED" = "$ACTUAL" ] ||
  die "checksum mismatch for $ASSET (expected $EXPECTED, got $ACTUAL)."

tar -xzf "$TMP/$ASSET" -C "$TMP"
SRC=$(find "$TMP" -type f -name agentpc -perm -u+x | head -n 1)
[ -n "$SRC" ] || die "the archive does not contain an agentpc binary."

# --- install ---------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
# Copy then rename, so a running agentpc (e.g. an open MCP session) keeps its old inode.
cp "$SRC" "$INSTALL_DIR/.agentpc.new"
chmod 755 "$INSTALL_DIR/.agentpc.new"
mv -f "$INSTALL_DIR/.agentpc.new" "$INSTALL_DIR/agentpc"
BIN="$INSTALL_DIR/agentpc"
say "Installed $BIN"

# --- PATH ------------------------------------------------------------------
shell_rc() {
  case "$(basename "${SHELL:-zsh}")" in
    bash) echo "$HOME/.bash_profile" ;;
    fish) echo "$HOME/.config/fish/config.fish" ;;
    zsh) echo "${ZDOTDIR:-$HOME}/.zshrc" ;;
    *) echo "$HOME/.profile" ;;
  esac
}

# add_line FILE LINE: append LINE unless FILE already has it.
add_line() {
  mkdir -p "$(dirname "$1")"
  touch "$1"
  grep -qxF "$2" "$1" || printf '\n%s\n' "$2" >>"$1"
}

RC=$(shell_rc)
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    case "$RC" in
      *.fish) add_line "$RC" "fish_add_path \"$INSTALL_DIR\"" ;;
      *) add_line "$RC" "export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
    esac
    say "Added $INSTALL_DIR to PATH in $RC (open a new terminal to pick it up)"
    PATH="$INSTALL_DIR:$PATH"
    ;;
esac

# --- QEMU ------------------------------------------------------------------
if ! has brew; then
  for b in /opt/homebrew/bin/brew /usr/local/bin/brew; do
    [ -x "$b" ] && eval "$("$b" shellenv)" && break
  done
fi

if has qemu-system-aarch64; then
  say "QEMU found: $(command -v qemu-system-aarch64)"
else
  if ! has brew; then
    say "QEMU is needed and comes from Homebrew, which is not installed."
    say "Installing Homebrew with its official installer; it asks for your macOS password once."
    if [ -r /dev/tty ]; then
      /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" </dev/tty
    else
      /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    fi
    [ -x /opt/homebrew/bin/brew ] || die "Homebrew install did not finish; install it from https://brew.sh and re-run."
    eval "$(/opt/homebrew/bin/brew shellenv)"
    # shellcheck disable=SC2016 # expanded later by the user's shell
    case "$RC" in
      *.fish) add_line "$RC" '/opt/homebrew/bin/brew shellenv | source' ;;
      *) add_line "$RC" 'eval "$(/opt/homebrew/bin/brew shellenv)"' ;;
    esac
  fi
  say "Installing QEMU (brew install qemu)"
  brew install qemu
  has qemu-system-aarch64 || die "qemu-system-aarch64 is still missing after brew install qemu."
fi

# --- MCP + doctor ----------------------------------------------------------
if [ "${AGENTPC_NO_MCP:-0}" != "1" ]; then
  say "Registering the agentpc MCP server with installed agents"
  "$BIN" mcp-install || warn "agentpc mcp-install failed; run it again later."
fi

say "Checking prerequisites (agentpc doctor)"
"$BIN" doctor || warn "agentpc doctor reported problems (see above)."

cat <<EOF

agentpc $VERSION is installed.

Next steps:
  agentpc bake ubuntu          # once, ~3 min: builds the Ubuntu golden image
  agentpc new ubuntu           # a fresh desktop in ~12 s
  Then ask your agent, e.g. "open a terminal on ubuntu-1 and run uname -a".

  Windows: download a Windows 11 ARM64 ISO from Microsoft, then
  agentpc bake windows --iso ~/Downloads/<file>.iso   # once, ~12 min

Docs: https://github.com/$REPO
EOF

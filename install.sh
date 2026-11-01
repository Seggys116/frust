#!/bin/sh
# FRUST_VERSION selects the release; FRUST_INSTALL_DIR overrides the default ~/.local/bin.
set -eu

REPO="Seggys116/frust"
BINS="frust fcargo fclippy frustc frustdoc frustup frustfmt frust-wrap"
DEST="${FRUST_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${FRUST_VERSION:-latest}"

say() { printf 'frust-install: %s\n' "$*"; }
die() { printf 'frust-install: error: %s\n' "$*" >&2; exit 1; }

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$1" -O "$2"
  else
    die "curl or wget is required"
  fi
}

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *) die "unsupported OS $(uname -s); on Windows use install.ps1" ;;
esac

case "$(uname -m)" in
  arm64 | aarch64) arch="aarch64" ;;
  x86_64 | amd64) arch="x86_64" ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac

target="$arch-$os"
archive="frust-$target.tar.gz"
if [ "$VERSION" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $archive ($VERSION)"
fetch "$base/$archive" "$tmp/$archive" || die "download failed; no release asset for $target"
fetch "$base/$archive.sha256" "$tmp/$archive.sha256" || die "checksum download failed"

expected="$(awk '{print $1}' "$tmp/$archive.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch (expected $expected, got $actual)"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$DEST"
for b in $BINS; do
  [ -f "$tmp/$b" ] || die "archive is missing $b"
  cp "$tmp/$b" "$DEST/$b.new"
  chmod 755 "$DEST/$b.new"
  mv -f "$DEST/$b.new" "$DEST/$b"
done

say "installed to $DEST"
case ":$PATH:" in
  *":$DEST:"*) ;;
  *) say "add it to PATH:  export PATH=\"$DEST:\$PATH\"" ;;
esac
say "run 'frust doctor' to check your environment"

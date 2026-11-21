#!/bin/sh
# Install NyPi from the public repository's GitHub Releases (decision D98):
#   curl -fsSL https://raw.githubusercontent.com/Nyporte/NyPi/main/scripts/install.sh | sh
# It picks the archive for this machine, checks it against the release's SHA256SUMS, puts the
# binary at ~/.nypi/bin/nypi (where the supervisor runs it, and `nypi update` replaces it), and
# links ~/.local/bin/nypi to it. NYPI_VERSION=v0.4.1 installs that version instead of the latest;
# NYPI_REPO=<owner>/<repo> installs from a fork.
set -eu

repo="${NYPI_REPO:-Nyporte/NyPi}"
version="${NYPI_VERSION:-}"
home="${NYPI_HOME:-$HOME/.nypi}"

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) echo "NyPi has no release for $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) echo "NyPi has no release for $(uname -m)" >&2; exit 1 ;;
esac
target="$arch-$os"

if [ -z "$version" ]; then
  # The latest release's tag, from where /releases/latest leads.
  version="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest" 2>/dev/null | sed 's|.*/tag/||' || true)"
fi
# No release yet: /releases/latest leads nowhere that names a tag.
case "$version" in
  "" | *://* | */*)
    echo "No release has been published at https://github.com/$repo/releases yet." >&2; exit 1 ;;
esac
case "$version" in v*) ;; *) version="v$version" ;; esac
file="nypi-${version#v}-$target.tar.gz"
base="https://github.com/$repo/releases/download/$version"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
echo "Downloading NyPi $version for $target…"
curl -fsSL -o "$work/$file" "$base/$file"
curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS"

expected="$(grep " $file\$" "$work/SHA256SUMS" | awk '{print $1}')"
if [ -z "$expected" ]; then
  echo "SHA256SUMS does not name $file" >&2; exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$work/$file" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$work/$file" | awk '{print $1}')"
fi
if [ "$expected" != "$actual" ]; then
  echo "the archive's SHA-256 is not the release's: nothing was installed" >&2; exit 1
fi

tar -xzf "$work/$file" -C "$work" nypi
mkdir -p "$home/bin" "$HOME/.local/bin"
chmod 755 "$work/nypi"
mv "$work/nypi" "$home/bin/nypi.new"
mv "$home/bin/nypi.new" "$home/bin/nypi"
ln -sf "$home/bin/nypi" "$HOME/.local/bin/nypi"
"$home/bin/nypi" --version

case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo "Add ~/.local/bin to your PATH to run nypi by name." ;;
esac
echo "Next: nypi hub up (this machine as the hub), or nypi node up (to join a hub)."

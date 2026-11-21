#!/usr/bin/env bash
# Builds the board's page (crates/nypi-board-ui) for WebAssembly into crates/nypi-cli/assets/board/,
# which `nypi` embeds at compile time. Run it, then rebuild nypi (`cargo build`).
#
# Needs the wasm32-unknown-unknown target and a wasm-bindgen CLI of the version Cargo.lock pins.
# When the default toolchain lacks the target and cannot take it (a root-owned ~/.rustup), the same
# Rust version is installed for the current user under the cache folder, with nothing else changed.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
case "$(uname)" in
    Darwin) cache="$HOME/Library/Caches/nypi-board" ;;
    *) cache="${XDG_CACHE_HOME:-$HOME/.cache}/nypi-board" ;;
esac
out="$root/crates/nypi-cli/assets/board"
target="wasm32-unknown-unknown"

version="$(rustc -V | awk '{print $2}')"
toolchain=()
if rustup target list --installed 2>/dev/null | grep -qx "$target"; then
    echo "Using the default toolchain's $target target."
else
    export RUSTUP_HOME="$cache/rustup"
    if ! rustup target list --toolchain "$version" --installed 2>/dev/null | grep -qx "$target"; then
        echo "Installing Rust $version with $target for this user in $RUSTUP_HOME"
        rustup toolchain install "$version" --profile minimal --target "$target" --no-self-update
    fi
    toolchain=("+$version")
fi

bindgen_version="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)"
bindgen="$(command -v wasm-bindgen || true)"
if [[ -z "$bindgen" || "$("$bindgen" --version | awk '{print $2}')" != "$bindgen_version" ]]; then
    bindgen="$cache/bin/wasm-bindgen"
    if [[ ! -x "$bindgen" || "$("$bindgen" --version | awk '{print $2}')" != "$bindgen_version" ]]; then
        echo "Installing wasm-bindgen-cli $bindgen_version in $cache"
        # With a toolchain of the script's own, RUSTUP_HOME has no default: name it.
        cargo "${toolchain[@]}" install wasm-bindgen-cli --version "$bindgen_version" --locked --root "$cache"
    fi
fi

cargo "${toolchain[@]}" build -p nypi-board-ui --target "$target" --profile wasm-release
rm -rf "$out"
mkdir -p "$out"
"$bindgen" --target web --no-typescript --remove-name-section --remove-producers-section --out-dir "$out" --out-name board \
    "target/$target/wasm-release/nypi_board_ui.wasm"
if command -v wasm-opt >/dev/null; then
    wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int "$out/board_bg.wasm" -o "$out/board_bg.wasm"
fi
cp crates/nypi-board-ui/static/* "$out/"
# Phones download the page; the board sends these when the browser takes gzip.
gzip -9 -k "$out/board_bg.wasm" "$out/board.js" "$out/board.css"
echo "Built the board page in $out ($(du -sh "$out" | awk '{print $1}')). Now rebuild nypi: cargo build"

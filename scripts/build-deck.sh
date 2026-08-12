#!/usr/bin/env bash
set -euo pipefail

ROOT="${ZED_WORKTREE_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

download_file() {
    local url="$1"
    local output="$2"

    mkdir -p "$(dirname "$output")"

    if command -v curl >/dev/null 2>&1; then
        curl -L -o "$output" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -O "$output" "$url"
    else
        echo "curl or wget is required to install the Decky CLI." >&2
        exit 1
    fi

    chmod +x "$output"
}

ensure_decky_cli() {
    if [[ -x "$ROOT/cli/decky" ]]; then
        return
    fi

    local os
    local arch
    local asset

    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os:$arch" in
        Darwin:x86_64)
            asset="decky-macOS-x86_64"
            ;;
        Darwin:arm64|Darwin:aarch64)
            asset="decky-macOS-aarch64"
            ;;
        Linux:x86_64)
            asset="decky-linux-x86_64"
            ;;
        Linux:arm64|Linux:aarch64)
            asset="decky-linux-aarch64"
            ;;
        *)
            echo "Unsupported system for automatic Decky CLI install: $os $arch" >&2
            exit 1
            ;;
    esac

    echo "Installing Decky CLI: $asset"
    download_file "https://github.com/SteamDeckHomebrew/cli/releases/latest/download/$asset" "$ROOT/cli/decky"
}

ensure_backend_checkout() {
    # `backend/` is the rust-engine submodule, and the decky CLI builds it in
    # Docker into the plugin's bin/. A clone without submodules would otherwise
    # produce a plugin that installs fine and has no generator — a failure that
    # only shows up on the Deck, at the moment someone picks a .CT file.
    if [[ ! -f "$ROOT/backend/Cargo.toml" ]]; then
        echo "backend/ is empty — fetching the rust-engine submodule."
        git -C "$ROOT" submodule update --init --recursive
    fi

    if [[ ! -f "$ROOT/backend/Cargo.toml" ]]; then
        echo "backend/Cargo.toml is still missing; the trainer generator cannot be built." >&2
        exit 1
    fi
}

if ! command -v pnpm >/dev/null 2>&1; then
    echo "pnpm is required. Install pnpm before building this plugin." >&2
    exit 1
fi

ensure_backend_checkout
ensure_decky_cli
pnpm i
"$ROOT/cli/decky" plugin build "$ROOT"

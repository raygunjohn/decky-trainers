#!/usr/bin/env bash
# Stages the binaries the DeckyTrainers Decky plugin ships in `bin/`.
#
# Two crates, two operating systems:
#
#   ct2trainer                Linux ELF   the generator, runs on the Deck
#   deckytrainers-x86_64.exe  Windows PE  embedded into every trainer, runs
#                                         under the game's Proton prefix
#
# `ct2trainer` packages a trainer by appending the table past the runtime PE's
# last section (see ct2trainer/src/overlay.rs), so it never parses or rewrites
# a PE and there is no host/target mismatch to manage beyond building the two
# targets here.
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$SRC/out"

export CARGO_HOME="${CARGO_HOME:-/cargo}"

# Container-local, *not* $SRC/target. The checkout is a bind mount from the
# host, and cargo's hardlink/rename traffic over Docker Desktop's file sharing
# loses freshly written rlibs: the build dies with `can't find crate for shlex
# which cc depends on` while compiling build dependencies. Costs a cold compile
# each run; a caching build belongs in a docker volume, which the decky CLI's
# `docker run` line doesn't let us add.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/htt-target}"

WIN_TARGET=x86_64-pc-windows-gnu

echo "==> building ct2trainer (release, linux)"
cargo build --release --manifest-path "$SRC/Cargo.toml" -p ct2trainer

echo "==> building trainer-runtime (release, $WIN_TARGET)"
cargo build --release --manifest-path "$SRC/Cargo.toml" -p trainer-runtime --target "$WIN_TARGET"

# Clear the *contents*, never the directory: the decky CLI bind-mounts `out/`
# to collect the artifacts, so removing it fails with "Device or resource busy".
mkdir -p "$OUT"
find "$OUT" -mindepth 1 -delete

# The Deck is x86-64 and nothing between here and the device checks: an ELF for
# the wrong architecture installs cleanly and fails at exec, with an error the
# plugin reports as "the generator would not run". e_machine (ELF header offset
# 18) is 62 for x86-64, 183 for aarch64.
require_amd64_elf() {
    local machine
    machine="$(od -An -t u1 -j 18 -N 1 "$1" | tr -d ' ')"
    if [ "$machine" != "62" ]; then
        echo "$1 is not an x86-64 ELF (e_machine=$machine) — the Deck cannot run it." >&2
        echo "Build with --platform=linux/amd64; see the Dockerfile." >&2
        exit 1
    fi
}

# Same check for the other side, and it matters more: a Linux `trainer-runtime`
# and a Windows `trainer-runtime.exe` differ by three characters in the path,
# and staging the wrong one produces trainers that Proton simply refuses to
# start. ct2trainer rejects a non-PE runtime too (overlay::reject_as_runtime),
# but failing here names the *build* rather than every later generation.
#
# PE: "MZ" at 0, e_lfanew (u32 LE at 0x3C) -> "PE\0\0", then Machine (u16 LE),
# 0x8664 for x86-64.
require_amd64_pe() {
    python3 - "$1" <<'PY'
import struct, sys
path = sys.argv[1]
with open(path, "rb") as fh:
    data = fh.read(0x400)
if data[:2] != b"MZ":
    sys.exit(f"{path} is not a PE (no MZ magic) — staged the Linux build by mistake?")
off = struct.unpack_from("<I", data, 0x3C)[0]
if data[off:off + 4] != b"PE\0\0":
    sys.exit(f"{path} has no PE header at e_lfanew={off:#x}")
machine = struct.unpack_from("<H", data, off + 4)[0]
if machine != 0x8664:
    sys.exit(f"{path} is a PE for machine {machine:#06x}, not x86-64 (0x8664)")
PY
}

require_amd64_elf "$CARGO_TARGET_DIR/release/ct2trainer"
require_amd64_pe  "$CARGO_TARGET_DIR/$WIN_TARGET/release/trainer-runtime.exe"

install -m 0755 "$CARGO_TARGET_DIR/release/ct2trainer" "$OUT/ct2trainer"

# `load_runtime` searches --runtime-dir for deckytrainers-x86_64.exe, then
# cheatengine-x86_64.exe, then trainer-runtime.exe. Any of the three resolves;
# the preferred name is used here so the staged file says what it is.
install -m 0755 "$CARGO_TARGET_DIR/$WIN_TARGET/release/trainer-runtime.exe" \
    "$OUT/deckytrainers-x86_64.exe"

# Optional: generate_trainer falls back to a minimal stub when absent.
if [ -f "$SRC/defines.lua" ]; then
    install -m 0644 "$SRC/defines.lua" "$OUT/defines.lua"
fi

echo "==> staged in $OUT:"
ls -la "$OUT"

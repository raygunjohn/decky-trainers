#!/usr/bin/env bash
#
# Builds the plugin zip and publishes it as a GitHub release asset, so the
# download URL can be pasted straight into Decky's "install from URL".
#
#   ./scripts/build-and-release.sh                 # tag from package.json
#   ./scripts/build-and-release.sh --tag v0.2.0
#   ./scripts/build-and-release.sh --draft         # publish, but not visible yet
#   ./scripts/build-and-release.sh --skip-build    # reuse out/DeckyTrainers.zip
#   ./scripts/build-and-release.sh --host-runtime  # no Docker; see below
#
# Re-running for an existing tag replaces the asset rather than failing, so a
# bad build can be corrected without deleting the release by hand.
#
# The zip is *verified* before anything is uploaded. A build with no Docker
# silently produces a plugin whose bin/ is missing the generator, and that only
# surfaces on the Deck when someone picks a table - by which point it looks like
# a broken plugin rather than a broken build.
set -euo pipefail

ROOT="${ZED_WORKTREE_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

PLUGIN_NAME="${PLUGIN_NAME:-DeckyTrainers}"
ZIP="$ROOT/out/$PLUGIN_NAME.zip"
TAG=""
NOTES_FILE=""
SKIP_BUILD=0
HOST_RUNTIME=0
DRAFT=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag) TAG="${2:?--tag needs a value}"; shift 2 ;;
        --notes-file) NOTES_FILE="${2:?--notes-file needs a path}"; shift 2 ;;
        --zip) ZIP="${2:?--zip needs a path}"; SKIP_BUILD=1; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        --host-runtime) HOST_RUNTIME=1; shift ;;
        --draft) DRAFT=1; shift ;;
        -h|--help) sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

die() { echo "error: $*" >&2; exit 1; }

command -v gh >/dev/null 2>&1 || die "the GitHub CLI (gh) is not installed"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated - run: gh auth login"

# ---------------------------------------------------------------- version ----

if [[ -z "$TAG" ]]; then
    VERSION="$(python3 -c 'import json; print(json.load(open("package.json"))["version"])')"
    TAG="v$VERSION"
fi
echo "==> release $TAG"

# A release is a thing other people install. Say plainly what it was cut from.
if [[ -n "$(git status --porcelain)" ]]; then
    echo "    note: working tree is dirty - the zip may not match any commit"
fi
echo "    commit $(git rev-parse --short HEAD) on $(git rev-parse --abbrev-ref HEAD)"

# ------------------------------------------------------------------ build ----

if [[ "$SKIP_BUILD" -eq 1 ]]; then
    echo "==> skipping build, using $ZIP"
elif [[ "$HOST_RUNTIME" -eq 1 ]]; then
    # No-Docker path. Cross-compiles the Windows runtime on this machine and
    # splices it into the existing zip, keeping ct2trainer as-is.
    #
    # This exists because the Docker build needs several GB and has filled a
    # laptop disk more than once, while the runtime cross-compiles in about
    # twenty seconds. It is a shortcut, not the reproducible path: it cannot
    # rebuild ct2trainer (a Linux ELF this host cannot link) and so requires a
    # previous zip to take it from. Prefer the default when Docker is healthy.
    [[ -f "$ZIP" ]] || die "--host-runtime needs an existing $ZIP to take ct2trainer from"
    command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 \
        || die "--host-runtime needs mingw-w64 (brew install mingw-w64)"

    echo "==> cross-compiling the runtime on this host"
    (cd backend && cargo build --release -p trainer-runtime --target x86_64-pc-windows-gnu)

    STAGE="$(mktemp -d)"
    trap 'rm -rf "$STAGE"' EXIT
    (cd "$STAGE" && unzip -q "$ZIP")
    cp backend/target/x86_64-pc-windows-gnu/release/trainer-runtime.exe \
       "$STAGE/$PLUGIN_NAME/bin/deckytrainers-x86_64.exe"
    chmod +x "$STAGE/$PLUGIN_NAME/bin/"*
    rm -f "$ZIP"
    (cd "$STAGE" && zip -q -r -X "$ZIP" "$PLUGIN_NAME")
    echo "    spliced a fresh runtime into $ZIP"
else
    echo "==> building (needs Docker)"
    ./scripts/build-deck.sh
fi

[[ -f "$ZIP" ]] || die "no zip at $ZIP"

# ----------------------------------------------------------------- verify ----
#
# Decky unzips this straight into its plugins directory, so the layout and the
# two binaries are the whole contract. Checking them here turns a silent bad
# build into a failed release.

echo "==> verifying $ZIP"
python3 - "$ZIP" "$PLUGIN_NAME" <<'PY'
import struct, sys, zipfile

zip_path, plugin = sys.argv[1], sys.argv[2]
z = zipfile.ZipFile(zip_path)
names = set(z.namelist())

def need(path):
    if f"{plugin}/{path}" not in names:
        sys.exit(f"error: zip is missing {plugin}/{path}")

for f in ("plugin.json", "package.json", "main.py", "dist/index.js",
          "py_modules/library.py", "py_modules/generate.py"):
    need(f)

# The generator is a Linux ELF; the runtime it embeds is a Windows PE. They are
# for different operating systems and swapping them produces a trainer Proton
# refuses to start, so check the actual headers rather than the file names.
ct = z.read(f"{plugin}/bin/ct2trainer")
if ct[:4] != b"\x7fELF":
    sys.exit("error: bin/ct2trainer is not an ELF - was this built without Docker?")
if ct[4] != 2 or struct.unpack_from("<H", ct, 18)[0] != 0x3E:
    sys.exit("error: bin/ct2trainer is not x86-64")

exe = z.read(f"{plugin}/bin/deckytrainers-x86_64.exe")
if exe[:2] != b"MZ":
    sys.exit("error: bin/deckytrainers-x86_64.exe is not a PE")
pe = struct.unpack_from("<I", exe, 0x3C)[0]
if exe[pe:pe+4] != b"PE\0\0" or struct.unpack_from("<H", exe, pe+4)[0] != 0x8664:
    sys.exit("error: the embedded runtime is not an x86-64 PE")

print(f"    {len(names)} entries, ct2trainer {len(ct):,} B (ELF), "
      f"runtime {len(exe):,} B (PE x86-64)")
PY

# ---------------------------------------------------------------- publish ----

REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"

NOTES_ARGS=()
if [[ -n "$NOTES_FILE" ]]; then
    NOTES_ARGS=(--notes-file "$NOTES_FILE")
elif [[ -f "$ROOT/RELEASE_NOTES.md" ]]; then
    NOTES_ARGS=(--notes-file "$ROOT/RELEASE_NOTES.md")
else
    NOTES_ARGS=(--generate-notes)
fi

URL="https://github.com/$REPO/releases/download/$TAG/$PLUGIN_NAME.zip"

if gh release view "$TAG" >/dev/null 2>&1; then
    echo "==> $TAG exists - replacing the asset"
    gh release upload "$TAG" "$ZIP" --clobber
else
    echo "==> creating $TAG"
    CREATE_ARGS=(--title "$TAG" "${NOTES_ARGS[@]}")
    [[ "$DRAFT" -eq 1 ]] && CREATE_ARGS+=(--draft)
    # Tagging the current commit is the point: a release nobody can trace back
    # to a revision is not much of a release.
    gh release create "$TAG" "$ZIP" --target "$(git rev-parse HEAD)" "${CREATE_ARGS[@]}"
fi

echo
echo "==> done. Paste this into Decky's install-from-URL:"
echo
echo "    $URL"
echo

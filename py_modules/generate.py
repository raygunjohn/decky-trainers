"""Building a trainer on the Deck from a cheat table.

Shells out to `bin/ct2trainer`, the Linux binary the decky CLI builds from the
`backend/` submodule (see `backend/entrypoint.sh`). It emits a **Windows PE**:
the trainer runs inside the game's Proton prefix, where it can use
`ReadProcessMemory`, `VirtualAllocEx` and `CreateRemoteThread` — the same
primitives such tools use — against the game.

Nothing here needs privilege. The plugin runs as the ordinary user and must
keep doing so: `wine` launched as root connects to *root's* wineserver, a
different session that cannot see the game's processes at all, which fails as
a trainer that starts fine and attaches to nothing.

Packaging is an overlay append, not PE surgery — `ct2trainer` copies the
runtime PE and puts the table after its last section — so generating a Windows
binary from Linux involves no resource-directory rewriting and has no
silent-failure mode. See `backend/ct2trainer/src/overlay.rs`.

No `decky` import: this runs straight from `python3` against a fake
`ct2trainer` (a shell script that writes a few bytes) — see CLAUDE.md.
Stdlib only.
"""

import hashlib
import os
import subprocess
import time

import library

#: Name of the generator inside the plugin's `bin/`.
GENERATOR = "ct2trainer"

#: A 1.9 MB table is the realistic worst case; this is a stuck-process guard,
#: not an expected duration.
GENERATE_TIMEOUT = 600.0

#: How much of ct2trainer's output to hand the UI. A CT parse failure names its
#: cause in the last line or two; the rest is progress logging.
LOG_TAIL_LINES = 14


def generator_paths(plugin_dir: str):
    """`(generator, runtime_dir)`. Both live in the plugin's `bin/`.

    `--runtime-dir` is where ct2trainer looks for the runtime PE to embed, which
    `entrypoint.sh` stages next to the generator as `deckytrainers-x86_64.exe`.
    Note the asymmetry: the generator is an ELF, the runtime beside it is a PE.
    """
    bin_dir = os.path.join(plugin_dir, "bin")
    return os.path.join(bin_dir, GENERATOR), bin_dir


def privilege_state() -> dict:
    """Whether the plugin is running as the user a trainer needs it to be.

    Replaces the old `ptrace_state`. The concern has inverted: a ptrace trainer
    needed *more* privilege than usual, and this one needs *less*. `wine` run
    as root talks to root's wineserver, so the trainer lands in a session with
    no game in it, sees nothing to attach to, and reports the game as missing.

    `canAttach` keeps its name and its meaning to callers ("a trainer launched
    now could reach the game"), so the panel's banner did not have to change
    shape — only what makes it true.
    """
    is_root = os.geteuid() == 0
    return {
        "isRoot": is_root,
        "canAttach": not is_root,
    }


def probe(plugin_dir: str) -> dict:
    """Is the generator there and runnable? Feeds the panel's banner.

    Everything about a missing or unusable generator is reported here rather
    than at generation time, because "nothing happened when I picked a table"
    is the one failure the user cannot diagnose from the QAM.
    """
    generator, runtime_dir = generator_paths(plugin_dir)
    state = dict(privilege_state())
    state["path"] = generator
    state["version"] = ""

    if not os.path.isfile(generator):
        state["ok"] = False
        state["error"] = (
            "the trainer generator is missing from this plugin build "
            f"({generator})"
        )
        return state

    if not os.access(generator, os.X_OK):
        state["ok"] = False
        state["error"] = f"the trainer generator is not executable ({generator})"
        return state

    runtime = _runtime_in(runtime_dir)
    if not runtime:
        state["ok"] = False
        state["error"] = (
            f"no trainer runtime alongside the generator in {runtime_dir} — "
            f"generation would fail at the last step"
        )
        return state

    try:
        result = subprocess.run(
            [generator, "--version"],
            capture_output=True, text=True, timeout=30.0,
        )
        state["version"] = (result.stdout or result.stderr or "").strip()
    except (OSError, subprocess.SubprocessError) as error:
        state["ok"] = False
        state["error"] = f"the trainer generator would not run: {error}"
        return state

    state["ok"] = True
    state["error"] = None
    return state


def _runtime_in(runtime_dir: str) -> str:
    """The runtime PE ct2trainer will embed, in its own search order.

    Must stay in step with `RUNTIME_CANDIDATES` in
    `backend/ct2trainer/src/pipeline.rs`; this exists so the panel can say the
    runtime is missing *before* a user picks a table and waits for a build.
    """
    for name in ("deckytrainers-x86_64.exe", "cheatengine-x86_64.exe",
                 "trainer-runtime.exe"):
        candidate = os.path.join(runtime_dir, name)
        if os.path.isfile(candidate):
            return candidate
    return ""


def _tail(text: str, lines: int = LOG_TAIL_LINES) -> str:
    kept = [line for line in (text or "").splitlines() if line.strip()]
    return "\n".join(kept[-lines:])


def sha256_of(path: str) -> str:
    digest = hashlib.sha256()
    try:
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(256 * 1024), b""):
                digest.update(chunk)
    except OSError:
        return ""
    return digest.hexdigest()


def generate(ct_path: str, dest_dir: str, name: str, plugin_dir: str,
             process_name: str = "", bits: int = 64,
             timeout: float = GENERATE_TIMEOUT, extra: dict = None) -> dict:
    """Build `dest_dir/<name>` from `ct_path`. Blocking — call in a thread.

    Writes to a temp file in the destination and renames only on success, so a
    generator that dies half-way never leaves something that looks installed.

    `extra` is merged into the sidecar. It exists so a table that arrived from
    somewhere — today, a GitHub repo — can record where, without there being a
    second generation path: a downloaded table is a path on disk like any
    other, and this is the *only* thing that distinguishes the trainer it
    produces.
    """
    generator, runtime_dir = generator_paths(plugin_dir)
    if not os.path.isfile(generator):
        return {"ok": False, "error": f"trainer generator missing ({generator})", "log": ""}
    if not os.path.isfile(ct_path):
        return {"ok": False, "error": f"no such table: {ct_path}", "log": ""}

    safe = library.trainer_filename(
        name or os.path.splitext(os.path.basename(ct_path))[0]
    )
    target = os.path.join(dest_dir, safe)
    temp_path = os.path.join(dest_dir, f".{safe}.part")

    try:
        os.makedirs(dest_dir, exist_ok=True)
    except OSError as error:
        return {"ok": False, "error": f"cannot create {dest_dir}: {error}", "log": ""}

    command = [
        generator,
        "--input", ct_path,
        "--output", temp_path,
        "--bits", str(bits),
        "--runtime-dir", runtime_dir,
    ]

    started = time.monotonic()
    try:
        result = subprocess.run(
            command, capture_output=True, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        _discard(temp_path)
        return {
            "ok": False,
            "error": f"the generator gave up after {int(timeout)}s on this table",
            "log": "",
        }
    except OSError as error:
        _discard(temp_path)
        return {"ok": False, "error": f"could not run the generator: {error}", "log": ""}

    # ct2trainer logs through tracing, which writes to stderr even when it
    # succeeds — so the tail is worth keeping either way.
    log = _tail(f"{result.stdout}\n{result.stderr}")
    elapsed = round(time.monotonic() - started, 1)

    if result.returncode != 0 or not os.path.isfile(temp_path):
        _discard(temp_path)
        return {
            "ok": False,
            "error": _reason(log, result.returncode),
            "log": log,
            "seconds": elapsed,
        }

    try:
        os.chmod(temp_path, 0o755)
        os.replace(temp_path, target)
    except OSError as error:
        _discard(temp_path)
        return {"ok": False, "error": f"could not install the trainer: {error}",
                "log": log, "seconds": elapsed}

    sidecar = {
        "gameName": (name or "").strip() or safe,
        "processName": (process_name or "").strip(),
        "version": "",
        "ctPath": ct_path,
        "ctSha256": sha256_of(ct_path),
        "installedAt": int(time.time()),
    }
    if extra:
        sidecar.update(extra)
    # Best effort: without it the trainer still launches, it just shows up as an
    # untagged local file. Never fail a built trainer over its metadata.
    written = library.write_sidecar(target, sidecar)

    return {
        "ok": True,
        "error": None,
        "path": target,
        "log": log,
        "seconds": elapsed,
        "entry": library.entry_for(target, sidecar if written else None),
    }


def _reason(log: str, code: int) -> str:
    """The generator's own last words, which name the cause far better than we can."""
    last = (log.splitlines() or [""])[-1].strip()
    if last:
        # tracing prefixes lines with a timestamp and level; the message is the tail.
        return last.split("ERROR", 1)[-1].strip(" :") or last
    return f"the generator failed (exit {code})"


def _discard(path: str):
    try:
        os.unlink(path)
    except OSError:
        pass

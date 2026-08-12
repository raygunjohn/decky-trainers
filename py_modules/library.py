"""The trainer library on disk — `~/trainers` and its metadata sidecars.

The directory *is* the state. A trainer is installed iff its binary is there;
there is no index to prune, nothing that can disagree with the disk, and
metadata that survives being copied to another Deck or a plugin reinstall.

No `decky` import, so this runs straight from a plain `python3` REPL
(see CLAUDE.md "Testing"). Stdlib only.

Trainers are now generated on the
Deck from a `.CT` file (see `generate.py`), so the fetch/search/rank half is
gone and only the on-disk half survives.
"""

import json
import os
import re
import stat

_SAFE_NAME = re.compile(r"[^A-Za-z0-9._-]+")

#: What a trainer is called. Generated trainers are Windows PEs that run under
#: the game's Proton prefix, so the extension is back to being the thing that
#: identifies one — and it is a far better test than the executable bit, which
#: a file copied off a FAT stick or unzipped without permissions loses.
TRAINER_SUFFIX = ".exe"

# Named for what they are rather than excluded by extension: with `.exe` as the
# positive test, the only sibling that could still be mistaken for a trainer is
# something that also ends in `.exe`. Kept as an explicit list because
# `defines.lua` and the `.json` sidecars sit in the same directory and a future
# reader will want to know they were considered.
#
# `defines.lua` is not hypothetical: every trainer writes one next to itself on
# startup ("Wrote .../defines.lua alongside the trainer"), so `trainerDir`
# always has one.
IGNORED_SUFFIXES = (".json", ".ct", ".lua", ".log", ".part", ".tmp")


def safe_filename(name: str, fallback: str = "trainer") -> str:
    """Never let a user-supplied name escape the trainer directory.

    Returns a bare name with no extension; `trainer_filename` adds the `.exe`.
    Splitting the two means a name the user typed can never *supply* its own
    extension and end up as `Foo.exe.exe` or, worse, `Foo.lua`.
    """
    base = os.path.basename((name or "").replace("\\", "/")).strip()
    base = _SAFE_NAME.sub("_", base).lstrip(".")
    if base.lower().endswith(TRAINER_SUFFIX):
        base = base[: -len(TRAINER_SUFFIX)]
    return base or fallback


def trainer_filename(name: str, fallback: str = "trainer") -> str:
    """`"Red Dead Redemption 2"` -> `"Red_Dead_Redemption_2.exe"`."""
    return safe_filename(name, fallback) + TRAINER_SUFFIX


def is_trainer_file(path: str) -> bool:
    """A generated (or hand-copied) trainer: a regular `.exe`.

    The executable bit is deliberately *not* required. A trainer is a Windows
    PE launched through Proton's `wine`, which does not care about the mode
    bits — and a file copied off a FAT stick, unzipped, or pulled over MTP
    arrives 0644. Requiring +x there would hide a perfectly launchable trainer
    from the list with nothing on screen to explain why.

    Sidecars, tables and `defines.lua` are excluded by name; none of them ends
    in `.exe`, so the suffix test alone would do it, but they are listed so a
    reader can see they were considered.
    """
    lowered = path.lower()
    if not lowered.endswith(TRAINER_SUFFIX) or lowered.endswith(IGNORED_SUFFIXES):
        return False
    try:
        info = os.stat(path)
    except OSError:
        return False
    return stat.S_ISREG(info.st_mode)


# ------------------------------------------------------------------ sidecars


def sidecar_path(trainer_path: str) -> str:
    """`.../rdr2` -> `.../rdr2.json`; `.../rdr2.exe` -> `.../rdr2.json`."""
    return os.path.splitext(trainer_path)[0] + ".json"


def write_sidecar(trainer_path: str, data: dict) -> bool:
    """Write `<name>.json` beside `<name>`. Returns whether it landed.

    Atomic, so a crash mid-write can't leave a half-parsed sidecar that would
    silently downgrade a generated trainer to an untagged one.
    """
    path = sidecar_path(trainer_path)
    temp_path = f"{path}.tmp"
    try:
        with open(temp_path, "w") as handle:
            json.dump(data, handle, indent=2)
        os.replace(temp_path, path)
        return True
    except OSError:
        try:
            os.unlink(temp_path)
        except OSError:
            pass
        return False


def read_sidecar(trainer_path: str):
    """The metadata beside a trainer, or None. Never raises — a missing or
    corrupt sidecar just means we don't know the game, not that we're broken."""
    try:
        with open(sidecar_path(trainer_path), "r") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def entry_for(trainer_path: str, sidecar=None, size: int = -1) -> dict:
    """One installed trainer as the frontend sees it.

    `source` is what the UI keys off: `generated` rows were built here from a
    `.CT` and carry a game name, a process name and the table they came from;
    `local` rows are a bare file someone copied in.
    """
    if sidecar is None:
        sidecar = read_sidecar(trainer_path)
    if size < 0:
        try:
            size = os.path.getsize(trainer_path)
        except OSError:
            size = 0

    exe = os.path.basename(trainer_path)
    known = isinstance(sidecar, dict)
    return {
        # Where the *table* came from, which is a different question from
        # `source` (where the trainer came from). Additive on purpose: a
        # trainer built from a downloaded table is an ordinary generated
        # trainer, and widening `source` would make it sort and render as a
        # special case for no gain. Empty unless the table was downloaded.
        "origin": (sidecar.get("origin") or "") if known else "",
        "repo": (sidecar.get("repo") or "") if known else "",
        # Generated trainers are keyed by path too: unlike a store record there
        # is no server-side id, and the path is what `delete_trainer` needs.
        "id": trainer_path,
        "gameName": (sidecar.get("gameName") or "") if known else "",
        "processName": (sidecar.get("processName") or "") if known else "",
        "version": (sidecar.get("version") or "") if known else "",
        "exe": exe,
        "path": trainer_path,
        "sizeMB": round(size / (1024 * 1024), 1),
        "installedAt": int(sidecar.get("installedAt") or 0) if known else 0,
        # The table this was built from, so a row can say what it is and a
        # rebuild doesn't need the file picker again.
        "ctPath": (sidecar.get("ctPath") or "") if known else "",
        "source": "generated" if known else "local",
    }

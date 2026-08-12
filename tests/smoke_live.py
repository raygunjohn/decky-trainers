"""Live smoke test: the real GitHub API, through the real `main.py`.

Everything in `test_remote.py` and `test_main.py` runs offline against stubs,
which cannot catch the things that only real data does — a table whose name has
a space in it, a listing cache round-tripping actual API JSON, a raw URL that
404s because of how a path was quoted. This is the closest thing to the Deck
that exists without a Deck.

The generator is still faked (a Rust toolchain is not the point here), so what
this proves is the path *up to* `ct2trainer` and the sidecar it leaves behind.

Needs network, and a repository to point at. Spends ~4 of the 60
unauthenticated API calls an hour per IP.

    cp .env.example .env      # then set SMOKE_REPO in it
    python3 tests/smoke_live.py

`SMOKE_REPO` names the repository to list. It lives in `.env` (gitignored)
rather than in this file so the tree carries no third-party repository
reference; unset, the test skips instead of failing.

Nothing else imports it and no other suite depends on it, so it is safe to skip
when offline — which is why it is a separate file rather than more of
`test_main.py`.
"""

import asyncio
import os
import shutil
import sys
import tempfile
import types
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HOME = tempfile.mkdtemp(prefix="smoke-home-")
PLUGIN = tempfile.mkdtemp(prefix="smoke-plugin-")

decky = types.ModuleType("decky")
decky.DECKY_USER_HOME = HOME
decky.DECKY_PLUGIN_SETTINGS_DIR = os.path.join(PLUGIN, "settings")
decky.DECKY_PLUGIN_RUNTIME_DIR = os.path.join(PLUGIN, "runtime")
decky.DECKY_PLUGIN_LOG_DIR = os.path.join(PLUGIN, "log")
decky.DECKY_PLUGIN_DIR = PLUGIN
decky.HOME = HOME
decky.DECKY_HOME = PLUGIN
decky.logger = types.SimpleNamespace(
    info=lambda *a, **k: None, error=lambda *a, **k: None,
    warning=lambda *a, **k: None, debug=lambda *a, **k: None,
)
sys.modules["decky"] = decky

sys.path.insert(0, os.path.join(ROOT, "py_modules"))
sys.path.insert(0, ROOT)

import library  # noqa: E402
import generate as generate_mod  # noqa: E402
import remote  # noqa: E402
from main import Plugin  # noqa: E402

def load_dotenv(path):
    """Minimal `KEY=VALUE` reader — this repo is stdlib-only, so no dotenv dep.

    A variable already in the real environment always wins, so
    `SMOKE_REPO=... python3 tests/smoke_live.py` overrides the file without
    anyone editing it. A missing file is not an error; the caller reports the
    one variable that actually matters.
    """
    try:
        with open(path, "r", encoding="utf-8") as handle:
            lines = handle.readlines()
    except OSError:
        return
    for line in lines:
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        key, value = key.strip(), value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        os.environ.setdefault(key, value)


load_dotenv(os.path.join(ROOT, ".env"))

#: The repository to list, from `SMOKE_REPO` (see `.env.example`). It is not
#: hardcoded because this tree deliberately carries no third-party repository
#: references — and because the useful target is a property, not one URL: public,
#: reachable anonymously, and holding `.CT`/`.ct` tables. Small, flat, and mixing
#: XML with pre-5.6 binary tables is what makes one worth listing, since that is
#: the mix `looks_like_legacy` exists to tell apart.
REPO = os.environ.get("SMOKE_REPO", "").strip()

PASS = 0
FAIL = 0


def check(label, got, want=True):
    global PASS, FAIL
    if got == want:
        PASS += 1
        print(f"  ok   {label}")
    else:
        FAIL += 1
        print(f"  FAIL {label}\n       got  {got!r}\n       want {want!r}")


def fake_generate(ct_path, dest_dir, name, plugin_dir, process_name="", bits=64,
                  timeout=0, extra=None):
    """Stands in for bin/ct2trainer, but reads the table the way it would."""
    with open(ct_path, "rb") as handle:
        head = handle.read(64)
    os.makedirs(dest_dir, exist_ok=True)
    target = os.path.join(dest_dir, library.trainer_filename(name))
    with open(target, "wb") as handle:
        handle.write(b"MZ" + head)
    sidecar = {"gameName": name, "processName": process_name, "version": "",
               "ctPath": ct_path, "ctSha256": generate_mod.sha256_of(ct_path),
               "installedAt": 1}
    if extra:
        sidecar.update(extra)
    library.write_sidecar(target, sidecar)
    return {"ok": True, "error": None, "path": target, "log": "fake build",
            "seconds": 0.0, "entry": library.entry_for(target, sidecar)}


async def main():
    if not REPO:
        print("  SKIP — SMOKE_REPO is not set")
        print("  (cp .env.example .env, then set it to a public repo of .CT"
              " tables; not a code failure)")
        return

    generate_mod.generate = fake_generate
    plugin = Plugin()
    await plugin._main()

    print(f"\n== listing {REPO} (live)")
    listing = await plugin.list_repo_tables(REPO)
    if not listing.get("ok"):
        print(f"  SKIP — could not list: {listing.get('error')}")
        print("  (rate limit or no network; not a code failure)")
        return
    tables = listing["tables"]
    check("listed something", len(tables) > 0, True)
    check("resolved a branch", bool(listing.get("ref")), True)
    check("pinned a commit", len(listing.get("commit") or ""), 40)
    check("not truncated", listing.get("truncated"), False)
    check("every row is a .ct", all(t["path"].lower().endswith(".ct") for t in tables), True)
    check("every row has a blob sha", all(len(t["sha"]) == 40 for t in tables), True)
    check("every row has a pinned url",
          all(listing["commit"] in t["url"] for t in tables), True)
    print(f"  ({len(tables)} tables at {listing['label']}@{listing['ref']})")

    print("\n== the listing cache is real JSON that round-trips")
    again = await plugin.list_repo_tables()
    check("second call is cached", again.get("cached"), True)
    check("cached listing is identical",
          [t["path"] for t in again["tables"]], [t["path"] for t in tables])

    print("\n== a raw URL from the listing actually resolves")
    # The quoting in `raw_url` is the part most likely to be subtly wrong, and
    # a name with a space is where it shows.
    spaced = next((t for t in tables if " " in t["name"]), tables[0])
    request = urllib.request.Request(spaced["url"], headers={"User-Agent": "smoke"})
    # Through `remote.ssl_context()`, not the default one. Python's default
    # context comes up with no usable roots in some environments (Decky's
    # plugin process on SteamOS, a python.org build on macOS without
    # `Install Certificates.command`), and this line used to fail there with
    # CERTIFICATE_VERIFY_FAILED while the rest of the suite passed — because
    # everything else already routes through `remote`.
    with urllib.request.urlopen(request, timeout=30,
                                context=remote.ssl_context()) as response:
        body = response.read()
    check(f"HTTP 200 for {spaced['name']!r}", response.status, 200)
    check("bytes match the size GitHub reported", len(body), spaced["size"])

    print("\n== build from the smallest table")
    smallest = min(tables, key=lambda t: t["size"])
    result = await plugin.generate_from_repo(smallest["path"], "Smoke Test", "smoke.exe")
    check(f"built from {smallest['name']!r}", result.get("ok"), True)
    if not result.get("ok"):
        print(f"       error: {result.get('error')}")
        return

    entry = result["entry"]
    check("source stays 'generated'", entry["source"], "generated")
    check("origin is github", entry["origin"], "github")
    check("repo recorded", entry["repo"], listing["label"])

    sidecar = library.read_sidecar(result["path"])
    check("commit in sidecar", sidecar["commit"], listing["commit"])
    check("repo path in sidecar", sidecar["ctRepoPath"], smallest["path"])
    check("url in sidecar", sidecar["ctUrl"], smallest["url"])
    check("cached table still on disk", os.path.isfile(sidecar["ctPath"]), True)

    print("\n== the cached table is byte-identical to the repo's blob")
    with open(sidecar["ctPath"], "rb") as handle:
        cached = handle.read()
    check("blob sha matches the tree entry", remote.blob_sha1(cached), smallest["sha"])

    print("\n== rebuilding costs no network")
    calls = []
    original = remote._get_bytes

    def counting(*args, **kwargs):
        calls.append(args[0] if args else "")
        return original(*args, **kwargs)

    remote._get_bytes = counting
    again = await plugin.generate_from_repo(smallest["path"], "Smoke Test 2", "smoke.exe")
    check("rebuild ok", again.get("ok"), True)
    check("rebuild downloaded nothing", len(calls), 0)
    remote._get_bytes = original

    print("\n== a table with an awkward name survives the whole path")
    if spaced is not smallest:
        result = await plugin.generate_from_repo(spaced["path"], "Spaced Name Test", "x.exe")
        check(f"built from {spaced['name']!r}", result.get("ok"), True)
        if not result.get("ok"):
            print(f"       error: {result.get('error')}")

    print("\n== local generation is untouched by any of this")
    ct = os.path.join(HOME, "local.ct")
    with open(ct, "wb") as handle:
        handle.write(b'<?xml version="1.0"?>\n<CheatTable><CheatEntries/></CheatTable>')
    result = await plugin.generate_trainer(ct, "Local Smoke", "local.exe")
    check("local build ok", result.get("ok"), True)
    check("local build has no origin", result["entry"]["origin"], "")
    check("local build has no repo", result["entry"]["repo"], "")

    print("\n== clearing the cache leaves the trainers alone")
    installed_before = len((await plugin.list_installed())["trainers"])
    await plugin.clear_table_cache()
    check("trainers survive", len((await plugin.list_installed())["trainers"]),
          installed_before)
    check("cached table is gone", os.path.isfile(sidecar["ctPath"]), False)


try:
    asyncio.run(main())
finally:
    shutil.rmtree(HOME, ignore_errors=True)
    shutil.rmtree(PLUGIN, ignore_errors=True)

print(f"\n{PASS} passed, {FAIL} failed")
sys.exit(1 if FAIL else 0)

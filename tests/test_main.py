"""Exercises main.py's new callables under a stubbed `decky`."""

import asyncio
import json
import os
import shutil
import sys
import tempfile
import types

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HOME = tempfile.mkdtemp(prefix="deck-home-")
PLUGIN = tempfile.mkdtemp(prefix="deck-plugin-")

decky = types.ModuleType("decky")
decky.DECKY_USER_HOME = HOME
decky.DECKY_PLUGIN_SETTINGS_DIR = os.path.join(PLUGIN, "settings")
decky.DECKY_PLUGIN_RUNTIME_DIR = os.path.join(PLUGIN, "runtime")
decky.DECKY_PLUGIN_LOG_DIR = os.path.join(PLUGIN, "log")
decky.DECKY_PLUGIN_DIR = PLUGIN
decky.HOME = HOME
decky.DECKY_HOME = PLUGIN
logger = types.SimpleNamespace(
    info=lambda *a, **k: None, error=lambda *a, **k: None,
    warning=lambda *a, **k: None, debug=lambda *a, **k: None,
)
decky.logger = logger
sys.modules["decky"] = decky

sys.path.insert(0, os.path.join(ROOT, "py_modules"))
sys.path.insert(0, ROOT)

import remote  # noqa: E402
import library  # noqa: E402
import generate as generate_mod  # noqa: E402
from main import Plugin  # noqa: E402
import main as main_mod  # noqa: E402

PASS = 0
FAIL = 0


def check(label, got, want=True):
    global PASS, FAIL
    if got == want:
        PASS += 1
    else:
        FAIL += 1
        print(f"  FAIL {label}\n       got  {got!r}\n       want {want!r}")


def section(name):
    print(f"\n== {name}")


XML = (b'<?xml version="1.0" encoding="utf-8"?>\n'
       b'<CheatTable CheatEngineTableVersion="18"></CheatTable>')

TREE = {
    "sha": "COMMITSHA",
    "truncated": False,
    "tree": [
        {"path": "tables/rdr2.CT", "type": "blob", "size": len(XML),
         "sha": remote.blob_sha1(XML)},
        {"path": "tables/other.ct", "type": "blob", "size": 4, "sha": "deadbeef"},
        {"path": "README.md", "type": "blob", "size": 3, "sha": "x"},
    ],
}

builds = []


def fake_generate(ct_path, dest_dir, name, plugin_dir, process_name="", bits=64,
                  timeout=0, extra=None):
    """Stands in for bin/ct2trainer: writes a few bytes and the sidecar."""
    builds.append({"ct_path": ct_path, "name": name, "process": process_name,
                   "extra": extra})
    os.makedirs(dest_dir, exist_ok=True)
    target = os.path.join(dest_dir, library.trainer_filename(name))
    with open(target, "wb") as handle:
        handle.write(b"MZ" + b"\0" * 64)
    sidecar = {"gameName": name, "processName": process_name, "version": "",
               "ctPath": ct_path, "ctSha256": "abc", "installedAt": 1}
    if extra:
        sidecar.update(extra)
    library.write_sidecar(target, sidecar)
    return {"ok": True, "error": None, "path": target, "log": "built", "seconds": 0.1,
            "entry": library.entry_for(target, sidecar)}


async def main():
    plugin = Plugin()
    await plugin._main()

    section("settings")
    settings = await plugin.get_settings()
    check("repoUrl default", settings["repoUrl"], "")
    check("githubToken default", settings["githubToken"], "")
    check("cache dir under runtime",
          plugin._table_cache_dir().startswith(decky.DECKY_PLUGIN_RUNTIME_DIR), True)
    check("cache dir is not trainerDir",
          plugin._table_cache_dir() != plugin._trainer_dir(), True)

    section("list_repo_tables")
    got = await plugin.list_repo_tables()
    check("no url set fails", got["ok"], False)
    check("no url names what to do", "URL" in got["error"], True)

    got = await plugin.list_repo_tables("https://gitlab.com/o/r")
    check("bad url fails", got["ok"], False)
    check("bad url not persisted", (await plugin.get_settings())["repoUrl"], "")

    calls = []

    def stub_json(url, token="", timeout=0):
        calls.append(url)
        if "/git/trees/" in url:
            return {"ok": True, "status": 200, "data": TREE,
                    "rateLimit": {"remaining": 57}, "error": None}
        return {"ok": True, "status": 200, "data": {"default_branch": "master"},
                "rateLimit": {"remaining": 58}, "error": None}

    remote._get_json = stub_json

    got = await plugin.list_repo_tables("https://github.com/o/r")
    check("listing ok", got["ok"], True)
    check("two tables", [t["path"] for t in got["tables"]],
          ["tables/other.ct", "tables/rdr2.CT"])
    check("repoUrl echoed", got["repoUrl"], "https://github.com/o/r")
    check("not cached first time", got["cached"], False)
    check("url persisted", (await plugin.get_settings())["repoUrl"],
          "https://github.com/o/r")

    before = len(calls)
    got = await plugin.list_repo_tables()
    check("second call served from cache", got["cached"], True)
    check("cache made no request", len(calls), before)
    check("cached listing has tables", len(got["tables"]), 2)

    got = await plugin.list_repo_tables(refresh=True)
    check("refresh refetches", len(calls) > before, True)
    check("refresh not cached", got["cached"], False)

    section("list_repo_tables degradation")

    def failing(url, token="", timeout=0):
        return {"ok": False, "status": 0, "data": None, "rateLimit": {},
                "error": "could not reach github.com (offline)"}

    remote._get_json = failing
    got = await plugin.list_repo_tables(refresh=True)
    check("offline falls back to cache", got["cached"], True)
    check("fallback marked stale", got["stale"], True)
    check("fallback warns", "could not reach" in (got["warning"] or ""), True)
    check("fallback still lists", len(got["tables"]), 2)

    section("generate_from_repo")
    remote._get_json = stub_json
    await plugin.list_repo_tables(refresh=True)

    fetched = []

    def stub_bytes(url, token="", timeout=0, cap=0):
        fetched.append(url)
        return {"ok": True, "status": 200, "data": XML, "error": None}

    remote._get_bytes = stub_bytes
    generate_mod.generate = fake_generate

    got = await plugin.generate_from_repo("tables/rdr2.CT", "Red Dead 2", "RDR2.exe")
    check("build ok", got["ok"], True)
    check("downloaded once", len(fetched), 1)
    check("built from the cached table", "tables" in builds[-1]["ct_path"], True)
    check("ct_path is in the cache",
          builds[-1]["ct_path"].startswith(plugin._table_cache_dir()), True)
    check("name sanitised for disk", builds[-1]["name"], "Red_Dead_2")
    check("process passed through", builds[-1]["process"], "RDR2.exe")

    extra = builds[-1]["extra"]
    check("origin recorded", extra["origin"], "github")
    check("repo recorded", extra["repo"], "o/r")
    check("ref recorded", extra["ref"], "master")
    check("commit recorded", extra["commit"], "COMMITSHA")
    check("repo path recorded", extra["ctRepoPath"], "tables/rdr2.CT")
    check("url recorded so the cache stays disposable",
          extra["ctUrl"].startswith("https://raw.githubusercontent.com/"), True)

    installed = await plugin.list_installed()
    row = next(t for t in installed["trainers"] if t["exe"] == "Red_Dead_2.exe")
    check("source stays 'generated'", row["source"], "generated")
    check("origin surfaced", row["origin"], "github")
    check("repo surfaced", row["repo"], "o/r")

    before = len(fetched)
    got = await plugin.generate_from_repo("tables/rdr2.CT", "Red Dead 2", "RDR2.exe")
    check("rebuild is a cache hit", len(fetched), before)
    check("rebuild ok", got["ok"], True)

    section("generate_from_repo refusals")
    got = await plugin.generate_from_repo("")
    check("no path fails", got["ok"], False)

    got = await plugin.generate_from_repo("tables/ghost.ct", "Ghost")
    check("unknown path fails", got["ok"], False)
    check("unknown path says refresh", "refresh" in got["error"], True)

    def bad_bytes(url, token="", timeout=0, cap=0):
        return {"ok": True, "status": 200, "data": b"<!DOCTYPE html><html>404</html>",
                "error": None}

    remote._get_bytes = bad_bytes
    got = await plugin.generate_from_repo("tables/other.ct", "Other")
    check("sha mismatch refused before building", got["ok"], False)
    check("nothing built", all(b["name"] != "Other" for b in builds), True)

    section("local generation is unchanged")
    ct = os.path.join(HOME, "Downloads", "local.ct")
    os.makedirs(os.path.dirname(ct), exist_ok=True)
    with open(ct, "wb") as handle:
        handle.write(XML)

    got = await plugin.generate_trainer(ct, "Local Game", "game.exe")
    check("local build ok", got["ok"], True)
    check("local build has no origin", builds[-1]["extra"], None)
    check("lastCtDir remembered", (await plugin.get_settings())["lastCtDir"],
          os.path.dirname(os.path.realpath(ct)))

    row = next(t for t in (await plugin.list_installed())["trainers"]
               if t["exe"] == "Local_Game.exe")
    check("local row is generated", row["source"], "generated")
    check("local row has empty origin", row["origin"], "")

    got = await plugin.generate_trainer(os.path.join(HOME, "nope.ct"), "X")
    check("missing table refused", got["ok"], False)
    got = await plugin.generate_trainer(ct.replace(".ct", ".txt"), "X")
    check("non-table refused", got["ok"], False)

    section("clear_table_cache")
    got = await plugin.clear_table_cache()
    check("cleared", got["ok"], True)
    check("freed something", got["freedMB"] >= 0, True)
    listing = await plugin.list_repo_tables()
    check("listing cache gone too, refetches", listing["cached"], False)


def fake_proc(port: int, owners):
    """A /proc with one LISTEN socket on `port` and the given owners:
    (pid, comm, argv0) triples, each holding a copy of the socket."""
    proc = tempfile.mkdtemp(prefix="deck-proc-")
    os.makedirs(os.path.join(proc, "net"))
    inode = 424242
    with open(os.path.join(proc, "net", "tcp"), "w") as handle:
        handle.write("  sl  local_address rem_address   st ...\n")
        handle.write(
            f"   0: 0100007F:{port:04X} 00000000:0000 0A 00000000:00000000 "
            f"00:00000000 00000000  1000        0 {inode} 1 0 100 0 0 10 0\n"
        )
    for pid, comm, argv0 in owners:
        os.makedirs(os.path.join(proc, pid, "fd"))
        with open(os.path.join(proc, pid, "comm"), "w") as handle:
            handle.write(comm + "\n")
        with open(os.path.join(proc, pid, "cmdline"), "wb") as handle:
            handle.write(argv0.encode() + b"\0")
        os.symlink(f"socket:[{inode}]", os.path.join(proc, pid, "fd", "7"))
    return proc


def socket_owner_tests():
    section("socket_owner_pids")
    # What the Deck actually shows: the trainer's wine process (comm "main")
    # and the game's wineserver both hold the listening socket.
    proc = fake_proc(42069, [
        ("100", "main", "Z:\\home\\deck\\trainers\\X.exe"),
        ("200", "wineserver", "/opt/proton/bin/wineserver"),
    ])
    check("wineserver is not an owner", main_mod.socket_owner_pids(42069, proc=proc), ["100"])
    check("is_wineserver by comm", main_mod.is_wineserver("200", proc), True)
    check("trainer is not a wineserver", main_mod.is_wineserver("100", proc), False)
    check("unknown pid is not a wineserver", main_mod.is_wineserver("999", proc), False)
    check("other ports have no owners", main_mod.socket_owner_pids(1, proc=proc), [])

    # Linux truncates comm to 15 bytes, which can split a UTF-8 character.
    # An unrelated process must not prevent us from finding the trainer.
    os.makedirs(os.path.join(proc, "300", "fd"))
    with open(os.path.join(proc, "300", "comm"), "wb") as handle:
        handle.write("background-proé".encode("utf-8")[:15] + b"\n")
    check("truncated UTF-8 comm does not abort the socket scan",
          main_mod.socket_owner_pids(42069, proc=proc), ["100"])
    check("unrelated process with truncated comm is not a wineserver",
          main_mod.is_wineserver("300", proc), False)
    os.symlink("/opt/proton/bin/wineserver", os.path.join(proc, "300", "exe"))
    check("truncated comm still allows wineserver detection by exe",
          main_mod.is_wineserver("300", proc), True)
    shutil.rmtree(proc, ignore_errors=True)

    check("Z: maps to /", main_mod.wine_path_to_unix("Z:\\home\\deck\\t\\X.exe"), "/home/deck/t/X.exe")
    check("other drives keep their letter", main_mod.wine_path_to_unix("C:\\x\\Y.exe"), "C:/x/Y.exe")

    section("_adopt_from_socket")
    plugin = Plugin()
    plugin.settings = {}
    plugin.proc = None
    saved = (main_mod.socket_owner_pids, main_mod.read_cmdline, os.readlink)
    main_mod.socket_owner_pids = lambda port, proc="/proc": ["100"]
    main_mod.read_cmdline = lambda pid: ["Z:\\home\\deck\\trainers\\X.exe"]
    os.readlink = lambda p: "/opt/proton/lib/wine/i386-unix/wine64-preloader"
    try:
        check("adopts", plugin._adopt_from_socket(), True)
        check("adopted name is the trainer, not the preloader", plugin.trainer_name, "X.exe")
        check("adopted path is the unix path", plugin.trainer_path, "/home/deck/trainers/X.exe")
        main_mod.read_cmdline = lambda pid: ["/usr/local/bin/native-trainer"]
        plugin._adopt_from_socket()
        check("non-wine trainer falls back to exe", plugin.trainer_name, "wine64-preloader")
    finally:
        main_mod.socket_owner_pids, main_mod.read_cmdline, os.readlink = saved


asyncio.run(main())
socket_owner_tests()
shutil.rmtree(HOME, ignore_errors=True)
shutil.rmtree(PLUGIN, ignore_errors=True)
print(f"\n{PASS} passed, {FAIL} failed")
sys.exit(1 if FAIL else 0)

"""Exercises main.py's new callables under a stubbed `decky`."""

import asyncio
import json
import os
import shutil
import sys
import tempfile
import types
import urllib.request

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


def api_token_tests():
    section("API token")
    env = {"WINEPREFIX": "/x"}
    token = main_mod.issue_api_token(env)
    check("token is long enough", len(token) >= 40, True)
    check("token lands in the trainer env", env[main_mod.API_TOKEN_ENV], token)
    check("game env untouched otherwise", env["WINEPREFIX"], "/x")
    check("tokens are unique per launch", main_mod.issue_api_token({}) != token, True)

    # http_json sends the header, and only when it has a token.
    captured = []

    class FakeResponse:
        status = 200

        def read(self):
            return b'{"ok": true}'

        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

    def fake_urlopen(request, timeout=None):
        captured.append(request)
        return FakeResponse()

    saved = urllib.request.urlopen
    urllib.request.urlopen = fake_urlopen
    try:
        main_mod.http_json("GET", "http://127.0.0.1:1/api/features", None, 1.0, "tok-123")
        check("header sent", captured[-1].get_header("X-trainer-token"), "tok-123")
        main_mod.http_json("POST", "http://127.0.0.1:1/api/x", {"a": 1}, 1.0, "")
        check("no header without a token", captured[-1].get_header("X-trainer-token"), None)
        check("still no Origin header", captured[-1].get_header("Origin"), None)
    finally:
        urllib.request.urlopen = saved

    # The token survives a plugin restart via the runtime dir, and a reset
    # removes it.
    plugin = Plugin()
    plugin.settings = {}
    plugin.proc = None
    plugin._save_token("persisted")
    mode = os.stat(plugin._token_path()).st_mode & 0o777
    check("token file is owner-only", oct(mode), oct(0o600))
    fresh = Plugin()
    fresh.settings = {}
    check("a new instance reads the token back", fresh._load_token(), "persisted")
    plugin._reset_trainer_state()
    check("reset clears the token", plugin.api_token, "")
    check("reset removes the file", os.path.exists(plugin._token_path()), False)
    check("missing file reads as no token", fresh._load_token(), "")


asyncio.run(main())
api_token_tests()
shutil.rmtree(HOME, ignore_errors=True)
shutil.rmtree(PLUGIN, ignore_errors=True)
print(f"\n{PASS} passed, {FAIL} failed")
sys.exit(1 if FAIL else 0)

"""Offline tests for py_modules/remote.py — replaces the two network seams."""

import hashlib
import json
import os
import shutil
import sys
import tempfile
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "py_modules"))
import remote  # noqa: E402

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


# ------------------------------------------------------------- parse_repo_url

section("parse_repo_url")
cases = [
    ("https://github.com/o/r", ("o", "r", "", "")),
    ("https://github.com/o/r/", ("o", "r", "", "")),
    ("http://github.com/o/r.git", ("o", "r", "", "")),
    ("https://www.github.com/o/r", ("o", "r", "", "")),
    ("github.com/o/r", ("o", "r", "", "")),
    ("o/r", ("o", "r", "", "")),
    # Surrounding whitespace is trimmed, and mixed case is preserved as typed.
    ("  https://github.com/MixedCase/RepoName  ", ("MixedCase", "RepoName", "", "")),
    ("https://github.com/o/r/tree/master", ("o", "r", "master", "")),
    ("https://github.com/o/r/tree/main/tables", ("o", "r", "main", "tables")),
    ("https://github.com/o/r/tree/main/tables/sub", ("o", "r", "main", "tables/sub")),
    # What the browser's address bar actually hands you.
    ("https://github.com/o/r?tab=readme-ov-file", ("o", "r", "", "")),
    ("https://github.com/o/r#readme", ("o", "r", "", "")),
    ("https://github.com/o/r/tree/main?foo=1", ("o", "r", "main", "")),
]
for url, want in cases:
    got = remote.parse_repo_url(url)
    check(f"parse {url!r}", got.get("ok"), True)
    if got.get("ok"):
        check(f"fields {url!r}",
              (got["owner"], got["repo"], got["ref"], got["subdir"]), want)

rejects = [
    "",
    "   ",
    "https://raw.githubusercontent.com/o/r/master/x.ct",
    "https://gist.github.com/o/abc123",
    "https://gitlab.com/o/r",
    "not a url at all",
    "https://github.com/o/r/blob/main/tables/x.ct",
]
for url in rejects:
    got = remote.parse_repo_url(url)
    check(f"reject {url!r}", got.get("ok"), False)
    check(f"reject {url!r} names a reason", bool(got.get("error")), True)

# The two file/gist cases must say what to paste instead, not just "no".
check("raw url message mentions repository",
      "repository" in remote.parse_repo_url(
          "https://raw.githubusercontent.com/o/r/master/x.ct")["error"], True)
check("gist message mentions repository",
      "repository" in remote.parse_repo_url("https://gist.github.com/o/abc")["error"], True)
check("blob url message mentions repository",
      "repository" in remote.parse_repo_url(
          "https://github.com/o/r/blob/main/x.ct")["error"], True)


# ------------------------------------------------------------------- raw_url

section("raw_url")
check("plain", remote.raw_url("o", "r", "abc", "tables/x.ct"),
      "https://raw.githubusercontent.com/o/r/abc/tables/x.ct")
check("space encoded", remote.raw_url("o", "r", "abc", "a b/c d.ct"),
      "https://raw.githubusercontent.com/o/r/abc/a%20b/c%20d.ct")
check("hash encoded", remote.raw_url("o", "r", "abc", "Game #2.CT"),
      "https://raw.githubusercontent.com/o/r/abc/Game%20%232.CT")
check("separators survive", remote.raw_url("o", "r", "abc", "a/b/c.ct"),
      "https://raw.githubusercontent.com/o/r/abc/a/b/c.ct")


# --------------------------------------------------------------- list_tables

TREE = {
    "sha": "COMMITSHA",
    "truncated": False,
    "tree": [
        {"path": "README.md", "type": "blob", "size": 10, "sha": "s0"},
        {"path": "tables", "type": "tree", "sha": "s1"},
        {"path": "tables/beta.CT", "type": "blob", "size": 200, "sha": "s2"},
        {"path": "tables/alpha.ct", "type": "blob", "size": 100, "sha": "s3"},
        {"path": "tables/deep/zeta.ct", "type": "blob", "size": 300, "sha": "s4"},
        {"path": "root.ct", "type": "blob", "size": 50, "sha": "s5"},
        {"path": "notes.ct.txt", "type": "blob", "size": 5, "sha": "s6"},
        {"path": "sub/x.ct", "type": "commit", "size": 1, "sha": "s7"},
    ],
}


def fake_json(mapping):
    """Replace _get_json with a dict of url-substring -> response."""
    def stub(url, token="", timeout=0):
        for key, value in mapping.items():
            if key in url:
                return value
        raise AssertionError(f"unexpected url {url}")
    return stub


ok_tree = {"ok": True, "status": 200, "data": TREE, "rateLimit": {"remaining": 58}, "error": None}
ok_repo = {"ok": True, "status": 200, "data": {"default_branch": "master"},
           "rateLimit": {"remaining": 59}, "error": None}

section("list_tables")
original_json = remote._get_json
remote._get_json = fake_json({"/git/trees/": ok_tree, "/repos/o/r": ok_repo})

got = remote.list_tables("o", "r")
check("ok", got["ok"], True)
check("resolved default branch", got["ref"], "master")
check("commit", got["commit"], "COMMITSHA")
check("only .ct blobs", [t["path"] for t in got["tables"]],
      ["root.ct", "tables/alpha.ct", "tables/beta.CT", "tables/deep/zeta.ct"])
check("case-insensitive suffix", any(t["name"] == "beta.CT" for t in got["tables"]), True)
check("ct.txt excluded", any("notes" in t["path"] for t in got["tables"]), False)
check("non-blob excluded", any(t["path"] == "sub/x.ct" for t in got["tables"]), False)
check("dir split", got["tables"][3]["dir"], "tables/deep")
check("name split", got["tables"][3]["name"], "zeta.ct")
check("size carried", got["tables"][1]["size"], 100)
check("url pinned to commit", "COMMITSHA" in got["tables"][0]["url"], True)
check("label", got["label"], "o/r")
check("truncated passthrough", got["truncated"], False)

# An explicit ref must not spend a request resolving the default branch.
calls = []


def counting(url, token="", timeout=0):
    calls.append(url)
    return ok_tree


remote._get_json = counting
got = remote.list_tables("o", "r", ref="dev")
check("explicit ref skips the repo call", len(calls), 1)
check("explicit ref used", "/git/trees/dev" in calls[0], True)

# subdir filter
remote._get_json = fake_json({"/git/trees/": ok_tree})
got = remote.list_tables("o", "r", ref="master", subdir="tables/deep")
check("subdir filters", [t["path"] for t in got["tables"]], ["tables/deep/zeta.ct"])

# truncated passthrough
truncated_tree = dict(TREE, truncated=True)
remote._get_json = fake_json(
    {"/git/trees/": {"ok": True, "status": 200, "data": truncated_tree,
                     "rateLimit": {}, "error": None}})
check("truncated true", remote.list_tables("o", "r", ref="m")["truncated"], True)

section("list_tables errors")
for status, needle in [(404, "not found"), (403, "403"), (451, "legal")]:
    remote._get_json = fake_json(
        {"": {"ok": False, "status": status, "data": None, "rateLimit": {}, "error": None}})
    got = remote.list_tables("o", "r", ref="m")
    check(f"HTTP {status} fails", got["ok"], False)
    check(f"HTTP {status} names {needle}", needle in got["error"].lower(), True)

remote._get_json = fake_json(
    {"": {"ok": False, "status": 403, "data": None,
          "rateLimit": {"remaining": 0, "reset": int(__import__("time").time()) + 1800},
          "error": None}})
got = remote.list_tables("o", "r", ref="m")
check("rate limit named", "rate limit" in got["error"].lower(), True)
check("rate limit says when", "minutes" in got["error"], True)

remote._get_json = fake_json(
    {"": {"ok": False, "status": 0, "data": None, "rateLimit": {},
          "error": "could not reach github.com (offline)"}})
got = remote.list_tables("o", "r", ref="m")
check("network error surfaces", "could not reach" in got["error"], True)

remote._get_json = fake_json(
    {"": {"ok": True, "status": 200, "data": {"nope": 1}, "rateLimit": {}, "error": None}})
check("garbage listing fails", remote.list_tables("o", "r", ref="m")["ok"], False)

remote._get_json = original_json


# -------------------------------------------------------------- blob + magic

section("TLS store resolution")
# Decky's plugin process comes up with no usable roots, so `remote` picks a CA
# bundle itself rather than trusting the default context. These are the ways
# that choice can go wrong.
check("missing file is not a store", remote._load_ca("/nonexistent/ca.pem"), None)
check("empty string is not a store", remote._load_ca(""), None)

empty_ca = os.path.join(tempfile.mkdtemp(prefix="ca-"), "empty.pem")
open(empty_ca, "w").close()
# `load_verify_locations` accepts an empty file without complaint; an empty
# store then fails exactly like a missing one, which is the bug being avoided.
check("empty file is not a store", remote._load_ca(empty_ca), None)

garbage_ca = os.path.join(os.path.dirname(empty_ca), "garbage.pem")
with open(garbage_ca, "w") as handle:
    handle.write("this is not a certificate\n")
check("garbage is not a store", remote._load_ca(garbage_ca), None)

context = remote.ssl_context()
check("a context is always returned", context is not None, True)
check("it verifies", context.verify_mode, __import__("ssl").CERT_REQUIRED)
check("hostname checking stays on", context.check_hostname, True)
check("the choice is recorded", bool(remote.ca_source()), True)
check("resolved once", remote.ssl_context() is context, True)

check("a cert failure names the cause",
      "certificate" in remote._tls_hint(Exception("boom")).lower(), True)
check("a cert failure names the store tried",
      remote.ca_source() in remote._tls_hint(Exception("boom")), True)

section("blob_sha1")
# Verified against the live API while planning: Amnesia.CT in
# grasmanek94/cheat-tables is 6296b500e2a96b81fff9ccfa4117771eab6d7a2c.
check("git blob formula", remote.blob_sha1(b"hello\n"),
      hashlib.sha1(b"blob 6\0hello\n").hexdigest())
check("empty blob", remote.blob_sha1(b""), hashlib.sha1(b"blob 0\0").hexdigest())

section("classify")
xml = b'<?xml version="1.0" encoding="utf-8"?>\n<CheatTable CheatEngineTableVersion="18">\n</CheatTable>'
check("xml", remote.classify(xml)["kind"], "xml")
check("xml ok", remote.classify(xml)["ok"], True)
check("bare CheatTable root", remote.classify(b'<CheatTable Ver="1"></CheatTable>')["kind"], "xml")

# `<?xml` alone is not a table.
rss = b'<?xml version="1.0"?><rss><channel></channel></rss>'
check("xml that is not a table is not 'xml'", remote.classify(rss)["kind"], "unknown")

legacy = b"CHEATENGINE" + (8).to_bytes(4, "little") + (3).to_bytes(4, "little") + b"\x00" * 40
check("legacy", remote.classify(legacy)["kind"], "legacy")
forked = b"SpuceEngine" + (7).to_bytes(4, "little") + (1).to_bytes(4, "little") + b"\x00" * 40
check("rebranded magic still legacy", remote.classify(forked)["kind"], "legacy")
old = b"CHEATENGINE" + (3).to_bytes(4, "little") + (1).to_bytes(4, "little") + b"\x00" * 40
check("version below 5.6 is not legacy", remote.classify(old)["kind"], "unknown")
huge = b"CHEATENGINE" + (8).to_bytes(4, "little") + (999999).to_bytes(4, "little") + b"\x00" * 40
check("absurd record count is not legacy", remote.classify(huge)["kind"], "unknown")
binary = bytes(range(11)) + (8).to_bytes(4, "little") + (3).to_bytes(4, "little") + b"\x00" * 40
check("unprintable magic is not legacy", remote.classify(binary)["kind"], "unknown")

lfs = (b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1234\n")
check("lfs detected", remote.classify(lfs)["kind"], "lfs")
check("lfs refused", remote.classify(lfs)["ok"], False)
check("lfs message names LFS", "LFS" in remote.classify(lfs)["error"], True)

check("html refused", remote.classify(b"<!DOCTYPE html><html><body>404")["ok"], False)
check("html kind", remote.classify(b"<html><body>nope</body></html>")["kind"], "html")
check("empty refused", remote.classify(b"")["ok"], False)

# A verified blob we cannot name is still the file the repo holds.
check("unknown accepted", remote.classify(b"\x01\x02\x03random bytes here")["ok"], True)
check("unknown kind", remote.classify(b"\x01\x02\x03random bytes here")["kind"], "unknown")


# ----------------------------------------------------------- download_table

section("download_table")
cache = tempfile.mkdtemp(prefix="ct-cache-")
payload = xml
entry = {"path": "tables/a b.ct", "name": "a b.ct", "dir": "tables",
         "size": len(payload), "sha": remote.blob_sha1(payload),
         "url": remote.raw_url("o", "r", "COMMIT", "tables/a b.ct")}

original_bytes = remote._get_bytes
fetches = []


def fake_bytes(data, status=200, ok=True, error=None):
    def stub(url, token="", timeout=0, cap=0):
        fetches.append(url)
        return {"ok": ok, "status": status, "data": data, "error": error}
    return stub


remote._get_bytes = fake_bytes(payload)
got = remote.download_table(entry, cache, "o", "r", "COMMIT")
check("download ok", got["ok"], True)
check("not cached first time", got["cached"], False)
check("kind", got["kind"], "xml")
check("landed on disk", os.path.isfile(got["path"]), True)
check("bytes on disk", open(got["path"], "rb").read(), payload)
check("path under cache", os.path.realpath(got["path"]).startswith(os.path.realpath(cache)), True)
check("commit in path", "COMMIT" in got["path"], True)
check("no .part left", os.path.exists(got["path"] + ".part"), False)

before = len(fetches)
again = remote.download_table(entry, cache, "o", "r", "COMMIT")
check("cache hit", again["cached"], True)
check("cache hit made no request", len(fetches), before)
check("cache hit same path", again["path"], got["path"])

# A different commit is a different cache entry.
other = remote.download_table(entry, cache, "o", "r", "OTHERCOMMIT")
check("different commit re-downloads", other["cached"], False)
check("different commit different path", other["path"] != got["path"], True)

section("download_table refusals")
bad = dict(entry, path="tables/mismatch.ct", sha="0" * 40)
remote._get_bytes = fake_bytes(payload)
got = remote.download_table(bad, cache, "o", "r", "COMMIT")
check("sha mismatch refused", got["ok"], False)
check("sha mismatch explains", "does not match" in got["error"], True)
check("sha mismatch wrote nothing", os.path.exists(
    remote.table_cache_path(cache, "o", "r", "COMMIT", "tables/mismatch.ct")), False)

pointer_entry = {"path": "tables/lfs.ct", "sha": remote.blob_sha1(lfs), "url": "u"}
remote._get_bytes = fake_bytes(lfs)
got = remote.download_table(pointer_entry, cache, "o", "r", "COMMIT")
check("lfs pointer refused", got["ok"], False)
check("lfs pointer named", "LFS" in got["error"], True)

html = b"<!DOCTYPE html><html>404</html>"
html_entry = {"path": "tables/h.ct", "sha": remote.blob_sha1(html), "url": "u"}
remote._get_bytes = fake_bytes(html)
check("html refused even when sha matches",
      remote.download_table(html_entry, cache, "o", "r", "COMMIT")["ok"], False)

remote._get_bytes = fake_bytes(b"", ok=False, error="that file is larger than 32 MB")
check("oversize surfaces",
      remote.download_table({"path": "big.ct", "sha": "", "url": "u"},
                            cache, "o", "r", "COMMIT")["ok"], False)

remote._get_bytes = fake_bytes(payload)
check("empty path refused", remote.download_table({}, cache, "o", "r", "C")["ok"], False)

# Blob with no sha in the tree entry: still downloadable, still classified.
no_sha = {"path": "tables/nosha.ct", "sha": "", "url": "u"}
check("missing sha still works", remote.download_table(no_sha, cache, "o", "r", "C")["ok"], True)

section("path containment")
escape = {"path": "../../../../etc/evil.ct", "sha": "", "url": "u"}
remote._get_bytes = fake_bytes(payload)
got = remote.download_table(escape, cache, "o", "r", "COMMIT")
check("traversal cannot escape",
      got["ok"] is False or os.path.realpath(got["path"]).startswith(os.path.realpath(cache)),
      True)
check("/etc untouched", os.path.exists("/etc/evil.ct"), False)

remote._get_bytes = original_bytes


# --------------------------------------------------------------- listing cache

section("listing cache")
listing = {"ok": True, "tables": [{"path": "a.ct"}], "fetchedAt": int(__import__("time").time())}
path = remote.listing_cache_path(cache, "o", "r", "master")
check("wrote", remote.write_listing_cache(path, listing), True)
check("read back", remote.read_listing_cache(path)["tables"], [{"path": "a.ct"}])
check("no .tmp left", os.path.exists(path + ".tmp"), False)

stale = dict(listing, fetchedAt=0)
remote.write_listing_cache(path, stale)
check("stale ignored", remote.read_listing_cache(path), None)
check("ttl -1 keeps stale", remote.read_listing_cache(path, ttl=-1) is not None, True)

with open(path, "w") as h:
    h.write("{not json")
check("corrupt cache is a miss, not a crash", remote.read_listing_cache(path), None)
check("missing cache is a miss", remote.read_listing_cache(os.path.join(cache, "nope.json")), None)

check("slug survives odd refs",
      "/" not in os.path.basename(remote.listing_cache_path(cache, "o", "r", "feat/x")), True)

section("clear_cache")
freed = remote.clear_cache(cache)
check("freed something", freed > 0, True)
check("tables gone", os.path.isfile(got.get("path") or "/nonexistent"), False)

shutil.rmtree(cache, ignore_errors=True)

print(f"\n{PASS} passed, {FAIL} failed")
sys.exit(1 if FAIL else 0)

"""Cheat tables from a GitHub repository.

A second *source* for a `.CT`, not a second way to build one. Everything here
ends at a path on disk, which is exactly what `generate.generate` already
takes — so a downloaded table and a table the user picked with the file picker
go through the same generator, the same sidecar and the same containment
checks. Nothing downstream knows which happened.

Listing is one request: `GET /repos/{owner}/{repo}/git/trees/{ref}?recursive=1`
returns every blob in the repository, subfolders included, with its path, size
and sha. A second request resolves the default branch when no ref was given.
That is 2 calls against an unauthenticated budget of 60/hour per IP, which is
why the listing is cached on disk and refreshed only on demand.

**Downloads are verified, not trusted.** `ct2trainer` never parses the table —
`build_cetrainer_bytes` XOR-wraps whatever bytes it is handed and appends them
as an overlay — so a bad download would build a trainer that fails only at
launch, inside Wine, in a log nobody opens from Gaming Mode. A git tree entry
carries the blob's sha, and a git blob sha is `sha1("blob <len>\\0" + content)`,
so `verify_blob` proves byte-for-byte that what arrived is what the repo holds.
That covers truncation, corruption and HTML error pages in one exact check.
The only case a *correct* blob still is not a table is Git LFS, where the blob
is a pointer file, so that one is detected by name.

No `decky` import, so this runs straight from `python3` (see CLAUDE.md
"Testing"); every network call goes through `_get_json`/`_get_bytes` so tests
can replace those two seams and drive the whole module offline. Stdlib only.
"""

import glob
import hashlib
import json
import os
import re
import ssl
import time
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.github.com"
RAW = "https://raw.githubusercontent.com"

#: GitHub rejects API requests with no User-Agent, and asks that it identify
#: the application rather than the HTTP library.
USER_AGENT = "DeckyTrainers-Decky/1.0"

#: A listing older than this is refetched on open. Table repos change slowly and
#: a stale list costs nothing but a missing table; the API budget is the scarce
#: resource, not freshness.
LISTING_TTL = 24 * 60 * 60

#: Runaway guard, not a limit anyone will meet: the largest table measured in
#: the wild is 2.5 MB.
MAX_TABLE_BYTES = 32 * 1024 * 1024

LIST_TIMEOUT = 30.0
DOWNLOAD_TIMEOUT = 120.0

TABLE_SUFFIX = ".ct"

_LFS_POINTER = b"version https://git-lfs.github.com/spec/v1"

# ------------------------------------------------------------------------ TLS
#
# Python's default SSL context comes up with **no usable roots** inside Decky's
# plugin process, and every request then dies with
# `CERTIFICATE_VERIFY_FAILED: unable to get local issuer certificate`. The same
# `remote.py`, run from a shell on the same Deck, verifies fine — so this is
# about what that process can reach, not about SteamOS.
#
# On SteamOS the paths OpenSSL defaults to (`/etc/ssl/cert.pem`,
# `/etc/ssl/certs/ca-certificates.crt`) are **symlinks** into
# `/etc/ca-certificates/extracted/`, which is where the real 220 KB bundle
# lives. A symlink that does not resolve for the plugin loads nothing, and
# loading nothing is indistinguishable from a server with a bad certificate.
#
# So: find a store that actually loads, prefer the OS's own (it stays current
# without us), and treat `certifi` as one candidate rather than a dependency —
# this plugin is stdlib-only by design and nothing here may require a pip
# install on the Deck.
#
# Verification is never disabled. This code downloads files that become
# executables; an unverified transport is not a trade worth making, and
# "it works now" would be the last anyone heard of it.

_CA_CANDIDATES = (
    # The extracted bundle itself, ahead of the symlinks that point at it.
    "/etc/ca-certificates/extracted/tls-ca-bundle.pem",
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ssl/cert.pem",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
)

#: Resolved once. `None` until the first request, then a context or the
#: default one; `_ca_source` records which, for the log line that makes this
#: diagnosable next time.
_ssl_context = None
_ca_source = ""


def _ca_files():
    """Every plausible CA bundle, best first.

    `SSL_CERT_FILE` wins because someone who set it meant it. `certifi` is
    tried by import *and* by path, since the plugin's interpreter may not have
    the system's `site-packages` on its search path even when the file is there.
    """
    env = os.environ.get("SSL_CERT_FILE") or ""
    if env:
        yield env

    for path in _CA_CANDIDATES:
        yield path

    try:
        import certifi  # noqa: PLC0415 - optional, absent on a stock Deck
        yield certifi.where()
    except Exception:
        pass

    for pattern in ("/usr/lib/python3*/site-packages/certifi/cacert.pem",
                    "/usr/lib/python3*/dist-packages/certifi/cacert.pem"):
        for path in sorted(glob.glob(pattern), reverse=True):
            yield path


def _load_ca(path: str):
    """A context using `path`, or None if it is not a usable store.

    `load_verify_locations` accepts an empty file without complaint, so the
    cert count is checked too — an empty store fails exactly the way a missing
    one does, which is the failure this whole section exists to end.
    """
    try:
        if not path or not os.path.isfile(path):
            return None
        context = ssl.create_default_context(cafile=path)
        if not context.get_ca_certs():
            return None
        return context
    except (OSError, ssl.SSLError, ValueError):
        return None


def ssl_context():
    """The context every request here uses. Resolved once, then reused."""
    global _ssl_context, _ca_source
    if _ssl_context is not None:
        return _ssl_context

    for path in _ca_files():
        context = _load_ca(path)
        if context is not None:
            _ssl_context, _ca_source = context, path
            return _ssl_context

    # Nothing loaded. The default context is still the right thing to hand to
    # urlopen — it may work where this probe could not look — and if it does
    # not, `_get_json` turns the resulting error into something actionable.
    _ssl_context = ssl.create_default_context()
    _ca_source = "system default"
    return _ssl_context


def ca_source() -> str:
    """Which store was chosen. Empty until the first request."""
    return _ca_source


def _tls_hint(error: Exception) -> str:
    """Say what a certificate failure means, rather than repeating OpenSSL."""
    return (
        "could not verify github.com's certificate — this Deck's trusted "
        f"certificate store could not be read (tried: {ca_source() or 'nothing'}). "
        f"Details: {error}"
    )

# ---------------------------------------------------------------- repo URLs


_REPO_URL = re.compile(
    r"^(?:https?://)?(?:www\.)?github\.com/"
    r"(?P<owner>[^/\s]+)/(?P<repo>[^/\s]+?)(?:\.git)?"
    r"(?:/tree/(?P<ref>[^/\s]+)(?:/(?P<subdir>.*))?)?/?$",
    re.IGNORECASE,
)

_SHORTHAND = re.compile(r"^(?P<owner>[\w.-]+)/(?P<repo>[\w.-]+?)(?:\.git)?/?$")


def parse_repo_url(url: str) -> dict:
    """`{ok, owner, repo, ref, subdir}` — or `{ok: False, error}`.

    Accepts `https://github.com/o/r`, `.../tree/<branch>`,
    `.../tree/<branch>/<subdir>`, a bare `o/r`, a `.git` suffix and a trailing
    slash. An empty `ref` means "whatever the default branch is".

    A `raw.githubusercontent.com` or `gist.github.com` URL is rejected *by
    name*: both are things a user will plausibly paste, and a bare "not a
    GitHub repo" leaves them with no idea what to paste instead.
    """
    text = (url or "").strip()
    if not text:
        return {"ok": False, "error": "paste a GitHub repository URL"}

    # `github.com/o/r?tab=readme-ov-file#readme` is what the browser's address
    # bar hands you, and neither part names anything we need.
    text = text.split("?", 1)[0].split("#", 1)[0].strip()

    lowered = text.lower()
    if "/blob/" in lowered:
        return {"ok": False, "error": "that links to one file — paste the repository URL "
                                      "(github.com/owner/repo)"}
    if "raw.githubusercontent.com" in lowered:
        return {"ok": False, "error": "that is a link to a single file — paste the repository URL "
                                      "(github.com/owner/repo)"}
    if "gist.github.com" in lowered:
        return {"ok": False, "error": "gists are not supported — paste a repository URL "
                                      "(github.com/owner/repo)"}

    match = _REPO_URL.match(text)
    if not match and "://" not in text and "github.com" not in lowered:
        match = _SHORTHAND.match(text)
    if not match:
        return {"ok": False, "error": f"{text} is not a GitHub repository URL"}

    found = match.groupdict()
    subdir = (found.get("subdir") or "").strip("/")
    return {
        "ok": True,
        "error": None,
        "owner": found["owner"],
        "repo": found["repo"],
        "ref": found.get("ref") or "",
        "subdir": subdir,
    }


def repo_label(owner: str, repo: str) -> str:
    return f"{owner}/{repo}"


# ------------------------------------------------------------------- the wire


def _headers(token: str = "", accept: str = "application/vnd.github+json") -> dict:
    headers = {"Accept": accept, "User-Agent": USER_AGENT}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def _rate_limit(response_headers) -> dict:
    """What is left of the API budget, for a message that can name the reset."""
    def number(name):
        try:
            return int(response_headers.get(name) or -1)
        except (TypeError, ValueError):
            return -1

    return {
        "limit": number("x-ratelimit-limit"),
        "remaining": number("x-ratelimit-remaining"),
        "reset": number("x-ratelimit-reset"),
    }


def _get_json(url: str, token: str = "", timeout: float = LIST_TIMEOUT) -> dict:
    """One of the two seams tests replace. `{ok, status, data, rateLimit, error}`."""
    request = urllib.request.Request(url, headers=_headers(token), method="GET")
    try:
        with urllib.request.urlopen(request, timeout=timeout,
                                    context=ssl_context()) as response:
            raw = response.read().decode("utf-8", "replace")
            status = response.status
            limits = _rate_limit(response.headers)
    except urllib.error.HTTPError as error:
        raw = ""
        try:
            raw = error.read().decode("utf-8", "replace")
        except Exception:
            pass
        status = error.code
        limits = _rate_limit(error.headers)
        return {"ok": False, "status": status, "data": _loads(raw),
                "rateLimit": limits, "error": None}
    except urllib.error.URLError as error:
        # A certificate failure is not "the network is down", and saying so
        # sends people to their router instead of to the actual problem.
        if isinstance(error.reason, ssl.SSLCertVerificationError):
            return {"ok": False, "status": 0, "data": None, "rateLimit": {},
                    "error": _tls_hint(error.reason)}
        return {"ok": False, "status": 0, "data": None, "rateLimit": {},
                "error": f"could not reach github.com ({error.reason})"}
    except ssl.SSLCertVerificationError as error:
        return {"ok": False, "status": 0, "data": None, "rateLimit": {},
                "error": _tls_hint(error)}
    except OSError as error:
        return {"ok": False, "status": 0, "data": None, "rateLimit": {},
                "error": f"could not reach github.com ({error})"}

    return {"ok": True, "status": status, "data": _loads(raw),
            "rateLimit": limits, "error": None}


def _loads(raw: str):
    if not raw:
        return None
    try:
        return json.loads(raw)
    except ValueError:
        return raw


def _get_bytes(url: str, token: str = "", timeout: float = DOWNLOAD_TIMEOUT,
               cap: int = MAX_TABLE_BYTES) -> dict:
    """The other seam. Streams with a hard cap so a wrong URL cannot fill the Deck."""
    request = urllib.request.Request(
        url, headers=_headers(token, accept="application/octet-stream"), method="GET"
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout,
                                    context=ssl_context()) as response:
            chunks = []
            total = 0
            while True:
                chunk = response.read(256 * 1024)
                if not chunk:
                    break
                total += len(chunk)
                if total > cap:
                    return {"ok": False, "status": response.status, "data": b"",
                            "error": f"that file is larger than {cap // (1024 * 1024)} MB — "
                                     f"refusing to download it"}
                chunks.append(chunk)
            return {"ok": True, "status": response.status, "data": b"".join(chunks),
                    "error": None}
    except urllib.error.HTTPError as error:
        return {"ok": False, "status": error.code, "data": b"",
                "error": f"the download failed (HTTP {error.code})"}
    except urllib.error.URLError as error:
        if isinstance(error.reason, ssl.SSLCertVerificationError):
            return {"ok": False, "status": 0, "data": b"",
                    "error": _tls_hint(error.reason)}
        return {"ok": False, "status": 0, "data": b"",
                "error": f"could not reach github.com ({error.reason})"}
    except ssl.SSLCertVerificationError as error:
        return {"ok": False, "status": 0, "data": b"", "error": _tls_hint(error)}
    except OSError as error:
        return {"ok": False, "status": 0, "data": b"", "error": f"the download failed ({error})"}


def _api_error(result: dict, owner: str, repo: str) -> str:
    """Turn a failed API call into something worth reading in Gaming Mode.

    Every one of these is a distinct message on purpose: a wrong guess here
    costs a reboot into desktop mode to diagnose.
    """
    if result.get("error"):
        return result["error"]

    status = result.get("status", 0)
    limits = result.get("rateLimit") or {}
    label = repo_label(owner, repo)

    if status == 404:
        return (f"{label} was not found — check the URL, or add a GitHub token "
                f"in settings if the repository is private")
    if status in (401, 403):
        if limits.get("remaining") == 0:
            return f"GitHub's rate limit is used up — it resets {_reset_phrase(limits)}"
        if status == 401:
            return "GitHub rejected the token in settings"
        return f"GitHub refused the request for {label} (HTTP 403)"
    if status == 451:
        return f"{label} is unavailable for legal reasons"
    return f"GitHub returned HTTP {status} for {label}"


def _reset_phrase(limits: dict) -> str:
    reset = limits.get("reset", -1)
    if reset and reset > 0:
        seconds = int(reset - time.time())
        if seconds > 60:
            return f"in about {max(1, round(seconds / 60))} minutes"
        if seconds > 0:
            return f"in under a minute"
    return "shortly"


# ------------------------------------------------------------------- listing


def default_branch(owner: str, repo: str, token: str = "") -> dict:
    result = _get_json(f"{API}/repos/{urllib.parse.quote(owner)}/"
                       f"{urllib.parse.quote(repo)}", token)
    if not result.get("ok"):
        return {"ok": False, "error": _api_error(result, owner, repo)}
    data = result.get("data")
    branch = (data or {}).get("default_branch") if isinstance(data, dict) else None
    if not branch:
        return {"ok": False, "error": f"could not tell which branch {repo_label(owner, repo)} uses"}
    return {"ok": True, "error": None, "ref": branch, "rateLimit": result.get("rateLimit") or {}}


def raw_url(owner: str, repo: str, commit: str, path: str) -> str:
    """A download URL pinned to a commit, not a branch.

    The file a user saw listed is then the file they get, and the cache key is
    exact. Path segments are percent-encoded individually — tables in the wild
    have spaces, `#` and accented characters in their names, and quoting the
    whole path would eat the separators.
    """
    encoded = "/".join(urllib.parse.quote(part, safe="") for part in path.split("/"))
    return (f"{RAW}/{urllib.parse.quote(owner)}/{urllib.parse.quote(repo)}/"
            f"{urllib.parse.quote(commit, safe='')}/{encoded}")


def is_table_path(path: str) -> bool:
    return path.lower().endswith(TABLE_SUFFIX)


def list_tables(owner: str, repo: str, ref: str = "", subdir: str = "",
                token: str = "") -> dict:
    """Every `.ct` blob in the repository, recursively.

    Returns `{ok, tables, ref, commit, truncated, rateLimit, error}`.
    `truncated` is passed straight through: above GitHub's tree ceiling the
    list is genuinely partial, and a silently short list of several thousand
    tables cannot be falsified from a QAM panel.
    """
    if not ref:
        resolved = default_branch(owner, repo, token)
        if not resolved.get("ok"):
            return {"ok": False, "error": resolved["error"], "tables": []}
        ref = resolved["ref"]

    url = (f"{API}/repos/{urllib.parse.quote(owner)}/{urllib.parse.quote(repo)}"
           f"/git/trees/{urllib.parse.quote(ref, safe='')}?recursive=1")
    result = _get_json(url, token)
    if not result.get("ok"):
        return {"ok": False, "error": _api_error(result, owner, repo), "tables": []}

    data = result.get("data")
    if not isinstance(data, dict) or not isinstance(data.get("tree"), list):
        return {"ok": False, "error": f"GitHub returned an unexpected listing for "
                                      f"{repo_label(owner, repo)}", "tables": []}

    commit = str(data.get("sha") or ref)
    prefix = f"{subdir.strip('/')}/" if subdir else ""

    tables = []
    for entry in data["tree"]:
        if not isinstance(entry, dict) or entry.get("type") != "blob":
            continue
        path = str(entry.get("path") or "")
        if not path or not is_table_path(path):
            continue
        if prefix and not path.startswith(prefix):
            continue
        directory, _, name = path.rpartition("/")
        tables.append({
            "path": path,
            "name": name,
            "dir": directory,
            "size": int(entry.get("size") or 0),
            "sha": str(entry.get("sha") or ""),
            "url": raw_url(owner, repo, commit, path),
        })

    # Folder then name, both case-insensitively: the panel renders them in this
    # order and a repo that groups tables per game should read that way.
    tables.sort(key=lambda t: (t["dir"].lower(), t["name"].lower()))

    return {
        "ok": True,
        "error": None,
        "tables": tables,
        "owner": owner,
        "repo": repo,
        "label": repo_label(owner, repo),
        "ref": ref,
        "commit": commit,
        "subdir": subdir,
        "truncated": bool(data.get("truncated")),
        "rateLimit": result.get("rateLimit") or {},
        "fetchedAt": int(time.time()),
    }


# --------------------------------------------------------------- listing cache


def _cache_slug(*parts) -> str:
    joined = "_".join(str(part) for part in parts if part)
    return re.sub(r"[^A-Za-z0-9._-]+", "-", joined).strip("-") or "repo"


def listing_cache_path(cache_dir: str, owner: str, repo: str, ref: str,
                       subdir: str = "") -> str:
    return os.path.join(cache_dir, "listings",
                        _cache_slug(owner, repo, ref or "default", subdir) + ".json")


def read_listing_cache(path: str, ttl: float = LISTING_TTL):
    """A cached listing, or None. Never raises — a corrupt cache means a refetch."""
    try:
        with open(path, "r") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict) or not isinstance(data.get("tables"), list):
        return None
    if ttl >= 0 and time.time() - float(data.get("fetchedAt") or 0) > ttl:
        return None
    return data


def write_listing_cache(path: str, listing: dict) -> bool:
    temp_path = f"{path}.tmp"
    try:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(temp_path, "w") as handle:
            json.dump(listing, handle)
        os.replace(temp_path, path)
        return True
    except OSError:
        try:
            os.unlink(temp_path)
        except OSError:
            pass
        return False


# ------------------------------------------------------------------ downloads


def blob_sha1(data: bytes) -> str:
    """Git's own object id for these bytes: `sha1("blob <len>\\0" + content)`.

    This is what makes a download verifiable rather than merely successful —
    the tree entry already told us the answer.
    """
    digest = hashlib.sha1()
    digest.update(b"blob %d\0" % len(data))
    digest.update(data)
    return digest.hexdigest()


def is_lfs_pointer(data: bytes) -> bool:
    """The one case where a *correct* blob still is not a table.

    A repository using Git LFS stores a ~130 byte text pointer as the blob, so
    it passes the sha check and then builds a trainer with a pointer file
    inside it.
    """
    return data[:128].lstrip().startswith(_LFS_POINTER)


# Ported from `ct_legacy::looks_like_legacy` in the runtime, deliberately down
# to the constants: matching the runtime's own detection means this accepts
# exactly the tables the trainer can go on to read.
_MAGIC_LEN = 11
_MIN_VERSION = 6
_MAX_RECORDS = 100_000

_XML_PREFIXES = (b"<?xml", b"<?XML", b"<CheatTable")


def looks_like_legacy(data: bytes) -> bool:
    """A pre-XML (5.6 and earlier) binary table.

    The 11-byte magic is *not* checked, exactly as the reference reader does not —
    rebranded forks ship `SpuceEngine`, `DAEngine\\0\\0\\0` and others. What
    identifies the format is a plausible version and record count in the two
    dwords that follow.
    """
    if len(data) < _MAGIC_LEN + 8:
        return False
    if data.startswith(_XML_PREFIXES):
        return False
    if not all(0x21 <= b <= 0x7E or b in (0, 0x20) for b in data[:_MAGIC_LEN]):
        return False
    version = int.from_bytes(data[_MAGIC_LEN:_MAGIC_LEN + 4], "little")
    records = int.from_bytes(data[_MAGIC_LEN + 4:_MAGIC_LEN + 8], "little")
    return _MIN_VERSION <= version <= 100 and records <= _MAX_RECORDS


def looks_like_xml_table(data: bytes) -> bool:
    head = data[:512].lstrip()
    if not head.startswith(_XML_PREFIXES):
        return False
    # `<?xml` alone is not enough — an API error, an RSS feed and a table all
    # start that way. The root element is what makes it a table.
    return b"<CheatTable" in data[:4096]


def classify(data: bytes) -> dict:
    """What these bytes are: `{kind, ok, error}`.

    `kind` is one of `xml`, `legacy`, `lfs`, `html`, `empty`, `unknown`. Only
    `lfs`, `html` and `empty` are refusals — an `unknown` blob whose sha
    matched is provably the file the repository holds, and refusing it would
    mean refusing a table format the runtime might read but this function has
    not heard of.
    """
    if not data:
        return {"kind": "empty", "ok": False, "error": "that file is empty"}
    if is_lfs_pointer(data):
        return {"kind": "lfs", "ok": False,
                "error": "this repository stores its tables in Git LFS, which is not supported"}

    head = data[:512].lstrip().lower()
    if head.startswith((b"<!doctype html", b"<html")):
        return {"kind": "html", "ok": False,
                "error": "GitHub returned a web page instead of the table"}

    if looks_like_xml_table(data):
        return {"kind": "xml", "ok": True, "error": None}
    if looks_like_legacy(data):
        return {"kind": "legacy", "ok": True, "error": None}
    return {"kind": "unknown", "ok": True, "error": None}


def table_cache_path(cache_dir: str, owner: str, repo: str, commit: str,
                     path: str) -> str:
    """`<cache>/<owner>/<repo>/<commit>/<path>`, pinned to the commit."""
    parts = [part for part in path.split("/") if part not in ("", ".", "..")]
    return os.path.join(cache_dir, _cache_slug(owner), _cache_slug(repo),
                        _cache_slug(commit or "head"), *parts)


def download_table(entry: dict, cache_dir: str, owner: str, repo: str,
                   commit: str, token: str = "", cap: int = MAX_TABLE_BYTES) -> dict:
    """Fetch one table into the cache. `{ok, path, cached, kind, bytes, error}`.

    A cache hit returns immediately. Otherwise the bytes are verified against
    the tree's blob sha, classified, and only then written — through
    `<file>.part` and `os.replace`, the same rule `generate.generate` follows,
    so nothing that looks complete exists until it is.
    """
    path = str(entry.get("path") or "")
    if not path:
        return {"ok": False, "error": "no table was selected", "path": ""}

    target = table_cache_path(cache_dir, owner, repo, commit, path)

    # The path came off the network. A git tree cannot contain `..`, but
    # `delete_trainer` and `generate_trainer` already set the precedent: a name
    # from outside is not permission to write anywhere on the Deck.
    root = os.path.realpath(cache_dir)
    if os.path.realpath(os.path.dirname(target)) != root and \
            not os.path.realpath(target).startswith(root + os.sep):
        return {"ok": False, "error": "that table's path would land outside the cache",
                "path": ""}

    if os.path.isfile(target) and os.path.getsize(target) > 0:
        return {"ok": True, "error": None, "path": target, "cached": True,
                "kind": "cached", "bytes": os.path.getsize(target)}

    url = entry.get("url") or raw_url(owner, repo, commit, path)
    result = _get_bytes(url, token, cap=cap)
    if not result.get("ok"):
        return {"ok": False, "error": result.get("error") or "the download failed", "path": ""}

    data = result.get("data") or b""

    # Exact, and it subsumes every "did the whole thing arrive" check: the tree
    # told us this blob's id before we asked for a single byte.
    expected = str(entry.get("sha") or "")
    if expected:
        actual = blob_sha1(data)
        if actual != expected:
            return {"ok": False, "path": "",
                    "error": f"the downloaded table does not match what {repo_label(owner, repo)} "
                             f"holds — it may have been changed mid-download"}

    verdict = classify(data)
    if not verdict["ok"]:
        return {"ok": False, "error": verdict["error"], "path": "", "kind": verdict["kind"]}

    temp_path = f"{target}.part"
    try:
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(temp_path, "wb") as handle:
            handle.write(data)
        os.replace(temp_path, target)
    except OSError as error:
        try:
            os.unlink(temp_path)
        except OSError:
            pass
        return {"ok": False, "error": f"could not save the table: {error}", "path": ""}

    return {"ok": True, "error": None, "path": target, "cached": False,
            "kind": verdict["kind"], "bytes": len(data)}


def clear_cache(cache_dir: str) -> int:
    """Drop every cached table and listing. Returns bytes reclaimed."""
    freed = 0
    for root, _, names in os.walk(cache_dir):
        for name in names:
            path = os.path.join(root, name)
            try:
                size = os.path.getsize(path)
                os.unlink(path)
                freed += size
            except OSError:
                continue
    return freed

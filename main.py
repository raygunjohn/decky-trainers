"""DeckyTrainers — Decky backend.

Builds trainers on the Deck from cheat tables (`bin/ct2trainer`, from
the `backend/` submodule), keeps them in ~/trainers alongside anything dropped
there by hand, finds the running game's process, launches a trainer against it
and proxies the trainer's REST API on 127.0.0.1:42069 to the frontend.

The trainer is a Windows PE and runs **inside the game's own Proton prefix**,
launched through the same `wine` binary the game is running under. That is not
a detail: process enumeration and `OpenProcess` only reach processes in the
same wineserver session, so a trainer started any other way — a different
prefix, a system wine, or as a different user — comes up healthy, sees no
processes, and reports the game as missing.

Which is why this module must **not** run as root (`plugin.json` carries no
`root` flag). Root gets root's wineserver.

Stdlib only — nothing to pip install on the Deck.
"""

import asyncio
import json
import os
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request

import decky

import generate
import library
import remote
import trainer_auth

LOG = "[DeckyTrainers]"

# How long the trainer gets to shut down cleanly before it is killed. It has to
# restore the game's original bytes and detach from every hooked site first;
# rushing that is how a stop takes the game with it.
SHUTDOWN_TICK = 0.5
SHUTDOWN_GRACE_TICKS = 12  # 6s

# Reads are the 2s poll and must not hang the panel. Actions are a different
# animal: activating an Auto Assembler script means AOB-scanning the game's
# whole address space, and a single `aobscanmodule` against RDR2 was measured at
# 82 seconds on a Deck. The old blanket 10s reported those as failures while the
# trainer was working perfectly — which is what made a busy trainer look dead,
# and invited a kill that risks the game.
READ_TIMEOUT = 15.0
ACTION_TIMEOUT = 120.0

DEFAULT_PORT = 42069
DEFAULT_TRAINER_DIR = os.path.join(decky.DECKY_USER_HOME, "trainers")
DEFAULT_CT_DIR = os.path.join(decky.DECKY_USER_HOME, "Downloads")

DEFAULT_SETTINGS = {
    "port": DEFAULT_PORT,
    "trainerDir": DEFAULT_TRAINER_DIR,
    # Where the file picker opens. Remembered per pick, because a .CT arrives
    # by browser or by scp and the second table is almost always beside the first.
    "lastCtDir": DEFAULT_CT_DIR,
    # The last table picked. This lives here rather than in the panel's state
    # because opening the file picker can unmount the QAM panel, and a pick that
    # only existed in React is gone by the time the panel comes back.
    "lastCtPath": "",
    # The GitHub repository the Repo tab lists. A second *source* for a table,
    # never a replacement for the picker: both end at a path on disk and go
    # through the same generator. One repo at a time, remembered, because
    # typing a URL on a gamepad is a cost worth paying once.
    "repoUrl": "",
    # Optional. Unauthenticated listing costs 2 calls against 60/hour per IP,
    # which is plenty; this is for private repos and for a shared IP that has
    # spent the budget elsewhere.
    "githubToken": "",
    "hideUnresolved": True,
}

# Wine's own plumbing shares the game's SteamGameId; never mistake it for the game.
WINE_SYSTEM_EXES = {
    "services.exe",
    "explorer.exe",
    "plugplay.exe",
    "winedevice.exe",
    "rpcss.exe",
    "svchost.exe",
    "tabtip.exe",
    "wineboot.exe",
    "winemenubuilder.exe",
    "start.exe",
    "conhost.exe",
    "cmd.exe",
    "rundll32.exe",
    "iexplore.exe",
    "steam.exe",
    "steamwebhelper.exe",
    "gameoverlayui.exe",
    "steamerrorreporter.exe",
    "steamerrorreporter64.exe",
    "unitycrashhandler64.exe",
    "unitycrashhandler32.exe",
    "crashpad_handler.exe",
    "python.exe",
}


# ---------------------------------------------------------------- /proc helpers


def iter_pids(proc: str = "/proc"):
    try:
        for entry in os.listdir(proc):
            if entry.isdigit():
                yield entry
    except OSError:
        return


def is_wineserver(pid, proc: str = "/proc") -> bool:
    """True for a wineserver.

    A wineserver holds a duplicate of every handle of every wine process it
    serves, the trainer's listening socket included, so a scan for the
    socket's owners finds the wineserver next to the trainer. The game's
    wineserver is the *game's* whole session: signalling it hangs the game.
    """
    try:
        # comm is byte-limited and can contain a truncated UTF-8 character.
        with open(f"{proc}/{pid}/comm", "rb") as handle:
            if handle.read().strip() == b"wineserver":
                return True
    except OSError:
        pass
    try:
        return os.path.basename(os.readlink(f"{proc}/{pid}/exe")) == "wineserver"
    except OSError:
        return False


def read_environ(pid: str) -> dict:
    with open(f"/proc/{pid}/environ", "rb") as handle:
        raw = handle.read()
    env = {}
    for chunk in raw.split(b"\0"):
        if not chunk:
            continue
        key, sep, value = chunk.partition(b"=")
        if sep:
            env[key.decode("utf-8", "replace")] = value.decode("utf-8", "replace")
    return env


def wine_path_to_unix(path: str) -> str:
    """`Z:\\home\\deck\\x.exe` -> `/home/deck/x.exe`; any other drive letter is
    left as a Windows path (only the name is useful then)."""
    unix = path.replace("\\", "/")
    if unix[:2].upper() == "Z:":
        return unix[2:] or "/"
    return unix


def read_cmdline(pid: str) -> list:
    with open(f"/proc/{pid}/cmdline", "rb") as handle:
        raw = handle.read()
    return [a.decode("utf-8", "replace") for a in raw.split(b"\0") if a]


def process_alive(pid) -> bool:
    return os.path.isdir(f"/proc/{pid}")


def read_rss_kb(pid: str) -> int:
    try:
        with open(f"/proc/{pid}/status", "r") as handle:
            for line in handle:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except (OSError, ValueError, IndexError):
        pass
    return 0


def is_proton_wrapper(cmdline) -> bool:
    """The `proton` python script runs as `proton waitforexitandrun <game>.exe`.

    Its argv therefore *contains* the game's path while the game itself may be
    long dead — scanning for a `.exe` argument finds the wrapper and reports a
    running game that isn't. Observed on the Deck: the plugin logged
    `(game Dishonored.exe, pid 8063)` while the real Dishonored.exe was a zombie.
    """
    for arg in cmdline:
        lowered = arg.lower()
        if lowered == "waitforexitandrun":
            return True
        if os.path.basename(lowered.replace("\\", "/")) == "proton":
            return True
    return False


def is_trainer_process(cmdline, exe_name: str, trainer_names) -> bool:
    """A trainer is never the game, but nothing else distinguishes them.

    A native trainer no longer inherits the game's environment, so it cannot
    carry the game's SteamGameId and this scan can't reach it — but a wine
    trainer left over from the store era still can. Such a trainer unpacks itself
    into `...\\Temp\\cetrainers\\...` and the original lives in the trainer
    directory; both used to look exactly like the game, and a second launch
    would adopt the *first trainer* as its target (observed:
    `(game dishonored_trainer_….exe, pid 13601)`).
    """
    if exe_name.lower() in trainer_names:
        return True
    return any("cetrainers" in arg.lower() for arg in cmdline)


def find_game_process(appid: str, trainer_names=()):
    """Return (pid, exe_name, environ) for the game Steam launched as `appid`.

    Launcher chains (PlayRDR2.exe -> RDR2.exe), the Proton wrapper and any
    trainer already running all carry the same SteamGameId — so pick by
    evidence: a process with WINEPREFIX in its env is a real wine process (the
    wrapper has only STEAM_COMPAT_DATA_PATH), and among those the game dwarfs
    any launcher in resident memory.

    Returns None when the game isn't actually running. That matters: Steam keeps
    the reaper and the Proton wrapper alive after a game crashes, so it still
    reports the app as running long after the process has gone.
    """
    appid = str(appid)
    trainer_names = {name.lower() for name in trainer_names}
    best = None
    best_score = None
    for pid in iter_pids():
        try:
            env = read_environ(pid)
            if appid not in (env.get("SteamGameId"), env.get("SteamAppId")):
                continue
            if "STEAM_COMPAT_DATA_PATH" not in env and "WINEPREFIX" not in env:
                continue  # native process (reaper, ...)
            cmdline = read_cmdline(pid)
            if is_proton_wrapper(cmdline):
                continue
            exe = next((a for a in cmdline if a.lower().endswith(".exe")), None)
            if not exe:
                continue  # also skips zombies, whose cmdline reads empty
            exe_name = os.path.basename(exe.replace("\\", "/"))
            if exe_name.lower() in WINE_SYSTEM_EXES:
                continue
            if is_trainer_process(cmdline, exe_name, trainer_names):
                continue
            score = ("WINEPREFIX" in env, read_rss_kb(pid))
            if best_score is None or score > best_score:
                best, best_score = (pid, exe_name, env), score
        except (OSError, UnicodeDecodeError):
            continue  # died mid-scan, or not ours to read
    return best


def _usable_wine(path: str) -> bool:
    """`wine64-preloader` is not a launcher — exec'ing it directly does nothing,
    and it is what /proc/<pid>/exe points at for a running Proton game."""
    if not path or "preloader" in os.path.basename(path):
        return False
    return os.path.isfile(path) and os.access(path, os.X_OK)


def resolve_wine(pid: str, env: dict):
    """The `wine` launcher of the Proton build the game is running under.

    Never a system wine: the trainer has to join the *game's* wineserver
    session, and a different wine build against the same prefix is at best a
    prefix upgrade and at worst two servers fighting over it.

    Four sources, in decreasing reliability. `WINELOADER` is authoritative when
    set but Proton Experimental does not set it; `/proc/<pid>/exe` is always
    there but points at the preloader, whose path has to be mapped back to the
    real launcher (two layouts, old and new); `STEAM_COMPAT_TOOL_PATHS` names
    the tool directory; and PATH is the last resort, filtered to
    `steamapps/common` so a system wine can never win.
    """
    candidates = []

    loader = env.get("WINELOADER")
    if loader:
        candidates.append(loader)

    try:
        exe_path = os.readlink(f"/proc/{pid}/exe")
    except OSError:
        exe_path = ""
    if exe_path:
        candidates.append(exe_path)
        # .../files/lib/wine/x86_64-unix/wine64-preloader -> .../files/bin/wine
        marker = "/lib/wine/"
        if marker in exe_path:
            candidates.append(os.path.join(exe_path.split(marker)[0], "bin", "wine"))
        # older layout: the preloader sits next to wine in bin/
        candidates.append(os.path.join(os.path.dirname(exe_path), "wine"))

    for root in (env.get("STEAM_COMPAT_TOOL_PATHS") or "").split(":"):
        if not root:
            continue
        candidates.append(os.path.join(root, "files", "bin", "wine"))
        candidates.append(os.path.join(root, "dist", "bin", "wine"))

    # Proton puts its own bin/ first on PATH; never fall back to a system wine.
    for entry in (env.get("PATH") or "").split(":"):
        if "/steamapps/common/" in entry:
            candidates.append(os.path.join(entry, "wine"))

    for candidate in candidates:
        if _usable_wine(candidate):
            return candidate
    return None


def wine_env_for(env: dict):
    """The game's environment, made safe to start a *second* wine process in.

    Returns `(env, error)`. Two things have to happen:

    * **Drop wine's per-process internals.** `WINESERVERSOCKET` is an inherited
      file descriptor number belonging to the game, and `WINELOADERNOEXEC` /
      `WINEPRELOADRESERVE` describe a load that already happened. Passing any
      of them on derails the new wine.
    * **Make `WINEPREFIX` explicit.** Without it wine falls back to `~/.wine` —
      a different prefix, a different wineserver, and a trainer that cannot see
      the game. The game's own env may only carry `STEAM_COMPAT_DATA_PATH`
      (the prefix's parent), so derive it.
    """
    env = dict(env)
    for var in ("WINESERVERSOCKET", "WINELOADERNOEXEC", "WINEPRELOADRESERVE"):
        env.pop(var, None)

    if "WINEPREFIX" not in env:
        compat = env.get("STEAM_COMPAT_DATA_PATH")
        if not compat:
            return None, "game process has no Proton prefix (no WINEPREFIX or STEAM_COMPAT_DATA_PATH)"
        env["WINEPREFIX"] = os.path.join(compat, "pfx")

    return env, None


def socket_owner_pids(port: int, proc: str = "/proc"):
    """PIDs owning a LISTEN socket on 127.0.0.1:<port>, via /proc/net/tcp.

    Never the wineserver, even though it holds the socket too (see
    `is_wineserver`). Adopting it as "the trainer" once ended with the plugin
    SIGTERMing the game's wineserver after the trainer had already exited,
    which froze the game and left the next trainer in an empty wine session
    where no game process could be found.
    """
    inodes = set()
    for path in (f"{proc}/net/tcp", f"{proc}/net/tcp6"):
        try:
            with open(path, "r") as handle:
                next(handle, None)  # header
                for line in handle:
                    fields = line.split()
                    if len(fields) < 10 or fields[3] != "0A":  # 0A = LISTEN
                        continue
                    local_port = fields[1].rsplit(":", 1)[-1]
                    if int(local_port, 16) == port:
                        inodes.add(fields[9])
        except (OSError, ValueError):
            continue
    if not inodes:
        return []

    owners = []
    targets = {f"socket:[{inode}]" for inode in inodes}
    for pid in iter_pids(proc):
        if is_wineserver(pid, proc):
            continue
        fd_dir = f"{proc}/{pid}/fd"
        try:
            for fd in os.listdir(fd_dir):
                try:
                    if os.readlink(os.path.join(fd_dir, fd)) in targets:
                        owners.append(pid)
                        break
                except OSError:
                    continue
        except OSError:
            continue
    return owners


# ------------------------------------------------------------------ HTTP (sync)


def _port_is_listening(port: int) -> bool:
    """Catch a listener that timed out or did not speak HTTP during the probe."""
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=1.0):
            return True
    except OSError:
        return False


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def http_json(method: str, url: str, body=None, timeout: float = 10.0,
              *, auth_secret: str):
    """Blocking JSON request. Returns (status, parsed_body_or_text)."""
    data = None
    headers = {"Accept": "application/json", "Authorization": f"Bearer {auth_secret}"}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        # Keep the credential on this loopback request, even if the environment
        # configures an HTTP proxy or a server responds with a redirect.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect())
        with opener.open(request, timeout=timeout) as response:
            payload = response.read().decode("utf-8", "replace")
            status = response.status
    except urllib.error.HTTPError as error:
        payload = error.read().decode("utf-8", "replace")
        status = error.code
    if not payload:
        return status, None
    try:
        return status, json.loads(payload)
    except ValueError:
        return status, payload


# ------------------------------------------------------------------- the plugin


class Plugin:
    async def _main(self):
        self.settings = dict(DEFAULT_SETTINGS)
        self._settings_path = os.path.join(
            decky.DECKY_PLUGIN_SETTINGS_DIR, "settings.json"
        )
        self._load_settings()
        self._auth_secret = trainer_auth.load_secret(decky.DECKY_PLUGIN_SETTINGS_DIR)

        self.proc = None
        self.trainer_name = ""
        self.trainer_path = ""
        self.prefix_path = ""
        self.game_exe = ""
        self.adopted = False
        self.sidecar_process = ""

        try:
            os.makedirs(self._trainer_dir(), exist_ok=True)
        except OSError as error:
            decky.logger.error(f"{LOG} could not create {self._trainer_dir()}: {error}")

        # A trainer must be launched by the same user as the game so that its
        # wine joins the game's wineserver session. Running as root is the one
        # way to break that, and it breaks it silently — the trainer starts,
        # binds its port, and finds no processes. Record the answer at startup
        # rather than infer it from a confusing failure later.
        state = generate.privilege_state()
        decky.logger.info(
            f"{LOG} started (trainers: {self._trainer_dir()}, uid: {os.geteuid()}, "
            f"can attach: {state['canAttach']})"
        )
        if not state["canAttach"]:
            decky.logger.warning(
                f"{LOG} running as root — a trainer launched from here would get "
                f"root's wineserver and see none of the game's processes. Remove "
                f"the 'root' flag from plugin.json."
            )

    async def _unload(self):
        decky.logger.info(f"{LOG} unloading — stopping trainer")
        try:
            await self.stop_trainer()
        except Exception as error:
            decky.logger.error(f"{LOG} stop on unload failed: {error}")

    async def _uninstall(self):
        await self._unload()

    # -- settings ------------------------------------------------------------

    def _load_settings(self):
        try:
            with open(self._settings_path, "r") as handle:
                stored = json.load(handle)
            if isinstance(stored, dict):
                self.settings.update(stored)
        except FileNotFoundError:
            pass
        except Exception as error:
            decky.logger.error(f"{LOG} could not read settings: {error}")

    def _save_settings(self):
        try:
            os.makedirs(os.path.dirname(self._settings_path), exist_ok=True)
            with open(self._settings_path, "w") as handle:
                json.dump(self.settings, handle, indent=2)
        except Exception as error:
            decky.logger.error(f"{LOG} could not write settings: {error}")

    def _port(self) -> int:
        try:
            return int(self.settings.get("port") or DEFAULT_PORT)
        except (TypeError, ValueError):
            return DEFAULT_PORT

    def _trainer_dir(self) -> str:
        return os.path.expanduser(
            self.settings.get("trainerDir") or DEFAULT_TRAINER_DIR
        )

    def _base_url(self) -> str:
        return f"http://127.0.0.1:{self._port()}"

    def _trainer_request(self, method: str, path: str, body=None,
                         timeout: float = 10.0):
        """Blocking authenticated transport, shared by probes and actions."""
        return http_json(method, self._base_url() + path, body, timeout,
                         auth_secret=self._auth_secret)

    async def get_settings(self):
        return dict(self.settings)

    async def set_setting(self, key: str, value):
        self.settings[key] = value
        self._save_settings()
        return dict(self.settings)

    # -- generation ----------------------------------------------------------

    async def check_generator(self):
        """Is `bin/ct2trainer` present, runnable, and able to produce a trainer
        that can attach? The Generate tab shows whatever this reports."""
        return await asyncio.to_thread(
            generate.probe, decky.DECKY_PLUGIN_DIR
        )

    async def generate_trainer(self, ct_path: str, name: str = "",
                               process_name: str = ""):
        """Build a trainer from a `.CT` file into `trainerDir`.

        `ct_path` comes from the frontend's file picker, so it is checked here
        rather than trusted; `name` becomes the filename, and is sanitised for
        the same reason `delete_trainer` re-checks its path — a name from the
        UI is not permission to write anywhere on the Deck.
        """
        ct_path = os.path.realpath(ct_path or "")
        if not ct_path or not os.path.isfile(ct_path):
            return {"ok": False, "error": "pick a .CT file first", "log": ""}
        if not ct_path.lower().endswith(".ct"):
            return {"ok": False, "error": f"{os.path.basename(ct_path)} is not a .CT table",
                    "log": ""}

        result = await self._build_from_table(ct_path, name, process_name)

        # Remembered for the next pick — tables arrive in batches, in one
        # folder. Only for a picked table: the download cache is not somewhere
        # anyone wants the file picker to open.
        if result.get("ok"):
            self.settings["lastCtDir"] = os.path.dirname(ct_path)
            self._save_settings()
        return result

    async def _build_from_table(self, ct_path: str, name: str = "",
                                process_name: str = "", extra: dict = None):
        """Build a trainer from a table already on disk.

        The single build path. A table picked with the file picker and a table
        downloaded from a repo differ only in how they got here and in `extra`,
        which records the latter's provenance in the sidecar — so the name
        checks, the containment rule, the running-trainer guard and the logging
        cannot drift between the two.
        """
        directory = self._trainer_dir()
        chosen = library.safe_filename(
            name or os.path.splitext(os.path.basename(ct_path))[0]
        )
        # The filename as it will actually be written, `.exe` and all. Comparing
        # the bare name here would mean the running-trainer guard below never
        # matched anything on disk.
        target = os.path.realpath(
            os.path.join(directory, library.trainer_filename(chosen))
        )
        if os.path.dirname(target) != os.path.realpath(directory):
            return {"ok": False, "error": "that name would land outside the trainer folder",
                    "log": ""}

        if os.path.isfile(target) and self.trainer_path and \
                os.path.realpath(self.trainer_path) == target and \
                await self._api_up(timeout=1.0):
            return {"ok": False, "error": "that trainer is running — kill it first",
                    "log": ""}

        decky.logger.info(f"{LOG} generating {chosen} from {ct_path}")
        result = await asyncio.to_thread(
            generate.generate, ct_path, directory, chosen,
            decky.DECKY_PLUGIN_DIR, process_name or "", 64,
            generate.GENERATE_TIMEOUT, extra,
        )

        if not result.get("ok"):
            decky.logger.error(f"{LOG} generation failed: {result.get('error')}")
            return result

        entry = result["entry"]
        if entry["source"] != "generated":
            decky.logger.warning(
                f"{LOG} {entry['exe']} built without its sidecar — it will show "
                f"as a local trainer"
            )
        decky.logger.info(
            f"{LOG} built {entry['exe']} ({entry['sizeMB']} MB) in "
            f"{result.get('seconds')}s"
        )
        return result

    # -- remote tables -------------------------------------------------------

    def _table_cache_dir(self) -> str:
        """Where downloaded tables and repo listings live.

        Deliberately not `trainerDir`: that directory *is* the installed-trainer
        state, and putting non-trainers in it weakens the invariant even though
        `is_trainer_file` would skip them. The runtime dir is disposable, which
        is exactly what a cache should be — the sidecar records `ctUrl`, so a
        rebuild after it is wiped can fetch the same bytes again.
        """
        return os.path.join(decky.DECKY_PLUGIN_RUNTIME_DIR, "tables")

    async def list_repo_tables(self, url: str = "", refresh: bool = False):
        """Every `.ct` in a GitHub repository, recursively.

        Cached on disk and served from there unless `refresh` is set: listing
        costs 2 of the 60 API calls an unauthenticated IP gets per hour, and
        that budget is shared with everything else on the network.
        """
        text = (url or self.settings.get("repoUrl") or "").strip()
        if not text:
            return {"ok": False, "error": "paste a GitHub repository URL",
                    "tables": [], "repoUrl": ""}

        parsed = remote.parse_repo_url(text)
        if not parsed.get("ok"):
            return {"ok": False, "error": parsed["error"], "tables": [], "repoUrl": text}

        # Persisted as soon as it parses, before anything can fail, for the same
        # reason `lastCtPath` is: opening a keyboard can unmount the panel, and
        # a URL that only existed in React is gone when it comes back.
        if text != self.settings.get("repoUrl"):
            self.settings["repoUrl"] = text
            self._save_settings()

        return await asyncio.to_thread(
            self._list_repo_tables_sync, parsed, text, bool(refresh)
        )

    def _list_repo_tables_sync(self, parsed: dict, url: str, refresh: bool):
        cache_dir = self._table_cache_dir()
        token = str(self.settings.get("githubToken") or "")
        owner, repo = parsed["owner"], parsed["repo"]
        cache_path = remote.listing_cache_path(
            cache_dir, owner, repo, parsed["ref"], parsed["subdir"]
        )

        if not refresh:
            cached = remote.read_listing_cache(cache_path)
            if cached:
                return dict(cached, repoUrl=url, cached=True, stale=False, warning=None)

        listing = remote.list_tables(
            owner, repo, parsed["ref"], parsed["subdir"], token
        )

        if not listing.get("ok"):
            # A listing we already have beats no listing at all: the tables in
            # it are still downloadable, and the panel says why it is old.
            cached = remote.read_listing_cache(cache_path, ttl=-1)
            if cached:
                decky.logger.warning(
                    f"{LOG} repo listing failed ({listing.get('error')}) — "
                    f"serving the cached one"
                )
                return dict(cached, repoUrl=url, cached=True, stale=True,
                            warning=listing.get("error"))
            decky.logger.error(f"{LOG} repo listing failed: {listing.get('error')}")
            return dict(listing, repoUrl=url, cached=False, stale=False)

        remote.write_listing_cache(cache_path, listing)
        decky.logger.info(
            f"{LOG} listed {len(listing['tables'])} tables in "
            f"{listing['label']}@{listing['ref']}"
            f"{' (truncated)' if listing.get('truncated') else ''} "
            f"[certs: {remote.ca_source()}]"
        )
        return dict(listing, repoUrl=url, cached=False, stale=False, warning=None)

    async def generate_from_repo(self, path: str, name: str = "",
                                 process_name: str = ""):
        """Download one table from the listed repo, then build it.

        Downloading is the only step this adds. Once the table is on disk it
        goes through `_build_from_table` exactly as a picked one does.
        """
        wanted = (path or "").strip()
        if not wanted:
            return {"ok": False, "error": "pick a table first", "log": ""}

        text = str(self.settings.get("repoUrl") or "").strip()
        parsed = remote.parse_repo_url(text)
        if not parsed.get("ok"):
            return {"ok": False, "error": parsed.get("error") or "no repository is set",
                    "log": ""}

        cache_dir = self._table_cache_dir()
        cache_path = remote.listing_cache_path(
            cache_dir, parsed["owner"], parsed["repo"], parsed["ref"], parsed["subdir"]
        )
        # Any age will do: the entry is what the user saw listed, and it is
        # pinned to the commit that listing came from.
        listing = remote.read_listing_cache(cache_path, ttl=-1)
        if not listing:
            return {"ok": False, "error": "that repo's table list has expired — refresh it",
                    "log": ""}

        entry = next((t for t in listing["tables"] if t.get("path") == wanted), None)
        if not entry:
            return {"ok": False, "error": f"{wanted} is no longer in that repo's listing — "
                                          f"refresh it", "log": ""}

        commit = str(listing.get("commit") or "")
        decky.logger.info(f"{LOG} fetching {wanted} from {listing.get('label')}@{commit[:8]}")
        downloaded = await asyncio.to_thread(
            remote.download_table, entry, cache_dir, parsed["owner"], parsed["repo"],
            commit, str(self.settings.get("githubToken") or ""),
        )
        if not downloaded.get("ok"):
            decky.logger.error(f"{LOG} download failed: {downloaded.get('error')}")
            return {"ok": False, "error": downloaded.get("error") or "the download failed",
                    "log": ""}

        decky.logger.info(
            f"{LOG} {'cached' if downloaded.get('cached') else 'downloaded'} "
            f"{downloaded.get('bytes', 0)} bytes ({downloaded.get('kind')})"
        )

        return await self._build_from_table(
            downloaded["path"],
            name or os.path.splitext(entry.get("name") or "")[0],
            process_name,
            extra={
                "origin": "github",
                "repo": listing.get("label") or f"{parsed['owner']}/{parsed['repo']}",
                "ref": listing.get("ref") or "",
                "commit": commit,
                "ctRepoPath": entry.get("path") or "",
                # What makes the cache genuinely disposable: the runtime dir
                # does not survive a plugin reinstall, and with the URL recorded
                # a rebuild can fetch the exact same bytes again.
                "ctUrl": entry.get("url") or "",
            },
        )

    async def clear_table_cache(self):
        """Drop every downloaded table and cached listing."""
        freed = await asyncio.to_thread(remote.clear_cache, self._table_cache_dir())
        decky.logger.info(f"{LOG} cleared {freed} bytes of table cache")
        return {"ok": True, "freedMB": round(freed / (1024 * 1024), 1)}

    async def list_installed(self):
        """Every trainer in `trainerDir`, labelled by its sidecar where there is one.

        Generated trainers carry `<name>.json`; anything copied in by hand
        doesn't, and shows up as `local` with just a filename. The directory is
        the source of truth — nothing to keep in sync with it.
        """
        return await asyncio.to_thread(self._list_installed_sync)

    def _list_installed_sync(self):
        directory = self._trainer_dir()
        try:
            names = os.listdir(directory) if os.path.isdir(directory) else []
        except OSError as error:
            decky.logger.error(f"{LOG} cannot list {directory}: {error}")
            names = []

        entries = []
        for name in names:
            path = os.path.join(directory, name)
            if not library.is_trainer_file(path):
                continue
            entries.append(library.entry_for(path))

        # Labelled trainers first, each group alphabetical: the ones we can name
        # are the ones worth showing at the top of a ~300px panel.
        entries.sort(
            key=lambda e: (
                e["source"] != "generated",
                (e["gameName"] or e["exe"]).lower(),
            )
        )
        return {"trainers": entries, "trainerDir": directory}

    def _trainer_names(self):
        """Every trainer in `trainerDir` — the set a game process can never be."""
        try:
            directory = self._trainer_dir()
            if not os.path.isdir(directory):
                return []
            return [
                name for name in os.listdir(directory)
                if library.is_trainer_file(os.path.join(directory, name))
            ]
        except OSError:
            return []

    def _installed_process(self) -> str:
        """`processName` from the sidecar of the trainer we're running.

        A table is built for one game, so the name recorded when the trainer was
        generated beats anything we can infer — it survives launcher chains
        (PlayRDR2.exe -> RDR2.exe) that fool a `/proc` scan. Tries the trainer's
        own path first, then its filename inside `trainerDir`, so a trainer we
        adopted rather than launched still resolves.
        """
        candidates = []
        if self.trainer_path:
            candidates.append(self.trainer_path)
        if self.trainer_name:
            candidates.append(os.path.join(self._trainer_dir(), self.trainer_name))

        for path in candidates:
            sidecar = library.read_sidecar(path)
            if sidecar and sidecar.get("processName"):
                return str(sidecar["processName"])
        return ""

    async def delete_trainer(self, trainer_id: str, path: str = ""):
        """Remove an installed trainer: the binary and its metadata sidecar."""
        target = path or ""
        if not target:
            return {"ok": False, "error": "nothing to delete"}

        # Only ever delete inside the directory we own; `path` comes from the
        # frontend, and a trainer id is not a capability to unlink anything.
        target = os.path.realpath(target)
        root = os.path.realpath(self._trainer_dir())
        if not target.startswith(root + os.sep):
            return {"ok": False, "error": "refusing to delete outside the trainer folder"}

        if self.trainer_path and os.path.realpath(self.trainer_path) == target and \
                await self._api_up(timeout=1.0):
            return {"ok": False, "error": "that trainer is running — kill it first"}

        try:
            os.unlink(target)
        except FileNotFoundError:
            pass  # already gone; still clear the sidecar below
        except OSError as error:
            decky.logger.error(f"{LOG} could not delete {target}: {error}")
            return {"ok": False, "error": str(error)}

        # An orphaned sidecar would relabel the next trainer to reuse the name.
        try:
            os.unlink(library.sidecar_path(target))
        except FileNotFoundError:
            pass
        except OSError as error:
            decky.logger.warning(f"{LOG} could not remove sidecar: {error}")

        decky.logger.info(f"{LOG} deleted {target}")
        return {"ok": True, "error": None}

    # -- game context --------------------------------------------------------

    async def get_game_context(self, appid: str):
        found = await asyncio.to_thread(
            find_game_process, appid, self._trainer_names()
        )
        if not found:
            return {"found": False}
        pid, exe_name, env = found
        return {
            "found": True,
            "pid": int(pid),
            "exeName": exe_name,
            "prefix": env.get("STEAM_COMPAT_DATA_PATH", ""),
            "protonWine": resolve_wine(pid, env) or "",
        }

    # -- launch --------------------------------------------------------------

    async def launch_trainer(self, appid: str, trainer_path: str):
        status = await self._api_status(timeout=1.0)
        if status in (401, 403):
            return {
                "ok": False,
                "error": (
                    f"the trainer on port {self._port()} rejected the authentication "
                    f"secret (HTTP {status}). Stop the running trainer and restart "
                    "the plugin before trying again."
                ),
            }
        if status is not None and 200 <= status < 300:
            return {"ok": False, "error": f"a trainer is already running on port {self._port()}"}
        if status is not None or await asyncio.to_thread(_port_is_listening, self._port()):
            detail = f" (HTTP {status})" if status is not None else ""
            return {
                "ok": False,
                "error": f"port {self._port()} is already in use{detail} — stop the "
                         "trainer or service using it before starting another trainer",
            }

        if not os.path.isfile(trainer_path):
            return {"ok": False, "error": f"trainer not found: {trainer_path}"}

        # The game is a hard precondition now, not just the source of the attach
        # hint: the trainer runs inside *its* prefix, through *its* wine, and
        # both come from this scan.
        found = await asyncio.to_thread(
            find_game_process, appid, self._trainer_names()
        )
        if not found:
            return {"ok": False, "error": "game process not found — is the game still running?"}
        pid, exe_name, env = found

        if os.geteuid() == 0:
            return {
                "ok": False,
                "error": (
                    "the plugin is running as root — wine would use root's "
                    "wineserver and the trainer would see no game. Remove the "
                    "'root' flag from plugin.json."
                ),
            }

        wine = resolve_wine(pid, env)
        if not wine:
            return {"ok": False, "error": "could not locate the game's Proton wine binary"}

        env, env_error = wine_env_for(env)
        if env_error:
            return {"ok": False, "error": env_error}
        env[trainer_auth.SECRET_ENV] = self._auth_secret

        decky.logger.info(
            f"{LOG} launching {trainer_path} via {wine} in prefix "
            f"{env.get('WINEPREFIX')} (game {exe_name}, pid {pid}, uid {os.geteuid()})"
        )

        try:
            self.proc = await asyncio.to_thread(
                _spawn, wine, trainer_path, env, os.path.dirname(trainer_path),
                os.path.join(decky.DECKY_PLUGIN_LOG_DIR, "trainer.log"),
                self._port(),
            )
        except Exception as error:
            decky.logger.error(f"{LOG} spawn failed: {error}")
            return {"ok": False, "error": f"could not start the trainer: {error}"}

        self.trainer_path = trainer_path
        self.trainer_name = os.path.basename(trainer_path)
        self.prefix_path = env.get("STEAM_COMPAT_DATA_PATH", "")
        self.game_exe = exe_name
        self.adopted = False

        # The trainer carries its table as an overlay and binds its port as soon
        # as it is up — no SFX chain to unpack first, unlike the store era. The
        # budget covers wine attaching to the existing wineserver, which is the
        # only slow part left.
        if not await self._wait_for_api(timeout=20.0):
            return {"ok": False, "error": "the trainer's REST API never came up"}

        self.sidecar_process = self._installed_process()

        # Attaching is the user's call, never ours. A guess that binds the
        # trainer to the wrong process fails in ways that look like the trainer
        # being broken — and the /proc scan can legitimately land on a launcher
        # (PlayRDR2.exe) rather than the game. So we only ever *suggest* a name;
        # the panel shows it in an editable field and waits for the button.
        hint = self.sidecar_process or exe_name or ""
        decky.logger.info(f"{LOG} launched; suggesting attach to {hint or '(nothing)'}")

        return {
            "ok": True,
            "error": None,
            "gameExe": exe_name,
            "attachHint": hint,
        }

    async def _api_status(self, timeout: float = 2.0):
        """HTTP status, including authentication failures; None if unavailable."""
        try:
            status, _ = await asyncio.to_thread(
                self._trainer_request, "GET", "/api/features", None, timeout
            )
            return status
        except Exception:
            return None

    async def _api_up(self, timeout: float = 2.0) -> bool:
        status = await self._api_status(timeout)
        return status is not None and 200 <= status < 300

    async def _wait_for_api(self, timeout: float = 30.0) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if await self._api_up(timeout=2.0):
                return True
            if self.proc is not None and self.proc.poll() is not None and \
                    not await self._trainer_procs():
                break  # the whole chain died — no point waiting out the timeout
            await asyncio.sleep(1.0)
        return False

    # -- detect / adopt ------------------------------------------------------

    async def detect_trainer(self):
        status, payload = None, None
        try:
            status, payload = await asyncio.to_thread(
                self._trainer_request, "GET", "/api/features", None, 2.0
            )
        except Exception:
            pass

        if status is None or not 200 <= status < 300:
            return {"apiUp": False, "attached": False}

        if not self.trainer_name:
            await asyncio.to_thread(self._adopt_from_socket)

        trainer_block = {}
        process_block = {}
        if isinstance(payload, dict):
            trainer_block = payload.get("trainer") or {}
            process_block = payload.get("process") or {}

        name = trainer_block.get("name") or trainer_block.get("exe") or self.trainer_name
        if name and not self.trainer_name:
            self.trainer_name = name

        game_exe = process_block.get("processName") or self.game_exe
        if game_exe:
            self.game_exe = game_exe

        self.sidecar_process = self._installed_process()

        return {
            "apiUp": True,
            "trainerName": self.trainer_name or name or "",
            "attached": bool(process_block.get("attached")),
            "gameExe": game_exe or "",
            # The sidecar's curated name wins: it's the one recorded when this
            # trainer was built, where `game_exe` may be whatever /proc showed.
            "attachHint": self.sidecar_process or game_exe or "",
        }

    def _adopt_from_socket(self):
        """Learn the name and path of a trainer we did not launch (blocking).

        The port's owner *is* the trainer. Under Proton it is a wine process,
        so `/proc/<pid>/exe` is the wine preloader and argv[0] is the trainer
        as wine spells it (`Z:\\home\\deck\\trainers\\X.exe`); that is the name
        the rest of the plugin matches processes by. `exe` is the fallback for
        a trainer that is not running under wine.
        """
        for pid in socket_owner_pids(self._port()):
            path = ""
            cmdline = []
            try:
                cmdline = read_cmdline(pid)
            except OSError:
                pass
            if cmdline and cmdline[0].lower().endswith(".exe"):
                path = wine_path_to_unix(cmdline[0])
            if not path:
                try:
                    path = os.readlink(f"/proc/{pid}/exe")
                except OSError:
                    pass
            if not path and cmdline:
                path = os.path.realpath(cmdline[0])
            if not path:
                continue

            self.trainer_name = os.path.basename(path)
            self.trainer_path = path
            self.adopted = True
            decky.logger.info(
                f"{LOG} adopted running trainer {self.trainer_name} (pid {pid})"
            )
            return True
        return False

    # -- status --------------------------------------------------------------

    async def get_trainer_status(self):
        api_up = await self._api_up(timeout=2.0)
        attached = False
        game_exe = self.game_exe
        if api_up:
            try:
                status, payload = await asyncio.to_thread(
                    self._trainer_request, "GET", "/api/features", None, 3.0
                )
                if 200 <= status < 300 and isinstance(payload, dict):
                    process_block = payload.get("process") or {}
                    attached = bool(process_block.get("attached"))
                    game_exe = process_block.get("processName") or game_exe
            except Exception:
                pass

        running = api_up or bool(await self._trainer_procs())
        return {
            "running": running,
            "apiUp": api_up,
            "attached": attached,
            "gameExe": game_exe or "",
            "trainerName": self.trainer_name,
            "adopted": self.adopted,
            "attachHint": self._installed_process() or game_exe or "",
        }

    # -- stop ----------------------------------------------------------------

    async def _trainer_procs(self):
        """PIDs belonging to the running trainer."""
        return await asyncio.to_thread(self._trainer_procs_sync)

    def _trainer_procs_sync(self):
        """The socket's owner, plus anything running the same trainer file.

        A native trainer is one process — no SFX chain to unwrap, and no wine
        prefix to match on, which is what the old scan used to separate a
        trainer from a same-named one in another prefix. What's left is the
        filename, and the port's owner is authoritative either way.
        """
        pids = set(socket_owner_pids(self._port()))
        if self.trainer_name:
            needle = self.trainer_name.lower()
            for pid in iter_pids():
                try:
                    cmdline = read_cmdline(pid)
                    if any(needle in arg.lower() for arg in cmdline):
                        pids.add(pid)
                except (OSError, UnicodeDecodeError):
                    continue
        if self.proc is not None and self.proc.poll() is None:
            pids.add(str(self.proc.pid))
        return pids

    async def stop_trainer(self):
        if not self.trainer_name:
            await asyncio.to_thread(self._adopt_from_socket)

        pids = await self._trainer_procs()
        if not pids:
            self._reset_trainer_state()
            return {"ok": True, "killed": 0, "apiUp": False, "unhooked": None}

        # Ask the trainer to shut itself down, and only fall back to signals if
        # that does not work.
        #
        # Signalling is unreliable here and the reason is worth keeping: the
        # trainer is a Windows process under Proton, its clean-shutdown path
        # waits on console control events (`spawn_shutdown_cleanup`), and a
        # Linux SIGTERM to the wine process does not dependably become one. A
        # trainer killed that way dies with its hooks still in the game, and the
        # *next* run's AOB scans then find their own `jmp` instead of the
        # pattern they want — every script fails to enable and it looks exactly
        # like a broken table.
        #
        # `POST /api/shutdown` un-patches and exits from inside the trainer,
        # where the ordering is actually controlled, and reports what it
        # restored. So a clean stop is confirmed rather than assumed.
        unhooked = None
        try:
            status, payload = await asyncio.to_thread(
                self._trainer_request, "POST", "/api/shutdown", {}, ACTION_TIMEOUT,
            )
            if 200 <= status < 300 and isinstance(payload, dict):
                unhooked = payload.get("deactivated", 0)
                decky.logger.info(
                    f"{LOG} trainer shut itself down: restored {unhooked} script(s), "
                    f"{payload.get('failed', 0)} failed"
                )
            else:
                decky.logger.warning(f"{LOG} /api/shutdown returned HTTP {status}")
        except Exception as error:
            # Expected whenever the trainer is already gone or wedged — which is
            # precisely when the kill path below is the right answer.
            decky.logger.info(f"{LOG} /api/shutdown unavailable ({error}); falling back to signals")

        # Give it a moment to actually exit before deciding it needs killing.
        # The endpoint replies *then* exits, so the process outlives the
        # response by a fraction of a second by design.
        if unhooked is not None:
            for _ in range(SHUTDOWN_GRACE_TICKS):
                if not await self._trainer_procs():
                    decky.logger.info(f"{LOG} trainer exited on its own; nothing to kill")
                    self._reset_trainer_state()
                    return {"ok": True, "killed": 0, "apiUp": False, "unhooked": unhooked}
                await asyncio.sleep(SHUTDOWN_TICK)
            decky.logger.warning(
                f"{LOG} trainer un-patched but is still running — signalling it"
            )

        # Fallback. Re-probe rather than reusing the set from the top: the most
        # common reason /api/shutdown fails is that the trainer had already
        # exited, and signalling PIDs that are gone would report a kill count
        # for work nobody did.
        pids = await self._trainer_procs()
        if not pids:
            decky.logger.info(f"{LOG} trainer already gone; nothing to signal")
            self._reset_trainer_state()
            return {"ok": True, "killed": 0, "apiUp": False, "unhooked": unhooked}

        # SIGTERM first, and mean it: SIGKILL is the last resort, not the first
        # move.
        signalled = 0
        for pid in pids:
            try:
                os.kill(int(pid), signal.SIGTERM)
                signalled += 1
            except (OSError, ValueError) as error:
                decky.logger.warning(f"{LOG} could not signal {pid}: {error}")

        survivors = set(pids)
        for _ in range(SHUTDOWN_GRACE_TICKS):
            survivors = {pid for pid in survivors if process_alive(pid)}
            if not survivors:
                break
            await asyncio.sleep(SHUTDOWN_TICK)

        killed = signalled
        for pid in survivors:
            decky.logger.warning(
                f"{LOG} {pid} ignored SIGTERM after "
                f"{SHUTDOWN_GRACE_TICKS * SHUTDOWN_TICK:.0f}s — killing it"
            )
            try:
                os.kill(int(pid), signal.SIGKILL)
            except (OSError, ValueError) as error:
                decky.logger.warning(f"{LOG} could not kill {pid}: {error}")
                killed -= 1

        decky.logger.info(
            f"{LOG} stopped {killed} trainer process(es) "
            f"({len(survivors)} needed SIGKILL)"
        )

        # Confirm by re-probing the port — it should refuse now.
        api_up = True
        for _ in range(10):
            await asyncio.sleep(0.3)
            api_up = await self._api_up(timeout=1.0)
            if not api_up:
                break

        if not api_up:
            self._reset_trainer_state()
        return {
            "ok": not api_up,
            "killed": killed,
            "apiUp": api_up,
            # None means the trainer never confirmed an un-patch, so the game
            # may still be hooked — which is what makes the *next* injection
            # fail rather than this one.
            "unhooked": unhooked,
            "error": None if not api_up else "trainer still answering after kill",
        }

    def _reset_trainer_state(self):
        self.proc = None
        self.trainer_name = ""
        self.trainer_path = ""
        self.prefix_path = ""
        self.game_exe = ""
        self.adopted = False
        self.sidecar_process = ""

    # -- API proxy -----------------------------------------------------------

    async def api_request(self, method: str, path: str, body=None):
        """Proxy a HATEOAS action to the trainer, sidestepping mixed-content rules."""
        if not path.startswith("/"):
            path = "/" + path
        method = method.upper()
        timeout = READ_TIMEOUT if method == "GET" else ACTION_TIMEOUT
        try:
            status, payload = await asyncio.to_thread(
                self._trainer_request, method, path, body, timeout
            )
        except Exception as error:
            decky.logger.error(f"{LOG} api_request {method} {path} failed: {error}")
            return {"ok": False, "status": 0, "error": str(error)}

        if not 200 <= status < 300:
            error = None
            if isinstance(payload, dict):
                error = payload.get("error")
            elif isinstance(payload, str):
                error = payload
            return {"ok": False, "status": status, "error": error or f"HTTP {status}"}

        return {"ok": True, "status": status, "data": payload}


def _spawn(wine: str, trainer_path: str, env: dict, cwd: str, log_path: str,
           port: int = DEFAULT_PORT):
    """Start `trainer_path` under `wine`, in the game's prefix.

    `env` must already be the game's environment as prepared by
    `wine_env_for` — passing the *plugin's* environment here would put the
    trainer in whatever prefix wine defaults to, which is the failure this
    whole path exists to avoid.
    """
    # The trainer's own output goes to a log next to the plugin's: when an
    # attach fails, that is the only place that says why.
    try:
        sink = open(log_path, "ab", buffering=0)
    except OSError:
        sink = subprocess.DEVNULL
    try:
        return subprocess.Popen(
            [wine, trainer_path, "--port", str(port)],
            cwd=cwd or None,
            env=env,
            stdout=sink,
            stderr=sink,
            start_new_session=True,  # survive the plugin loader
        )
    finally:
        if sink is not subprocess.DEVNULL:
            sink.close()  # the child keeps its own dup




#!/usr/bin/env python3
"""Record Steam's CEF console so a renderer crash leaves evidence behind.

A `steamwebhelper` segfault takes the whole Steam UI down — every Decky panel
with it — and the JS console dies with it. That is what made the 2026-08-10
crash undiagnosable: the coredump said `libcef.so` and nothing said what the
page was doing beforehand. This watches from outside the renderer and writes to
disk, so the next one is answerable.

Runs on the Deck. **Stdlib only** — SteamOS has an immutable rootfs and nothing
here may need a pip install, so the WebSocket client is implemented inline
rather than pulling in `websocket-client`.

    python3 cef-watch.py --once            # foreground, tee to stdout
    python3 cef-watch.py --log ~/cef.log   # what the systemd unit runs

Attaches to the **browser-level** endpoint and uses flattened sessions, so one
connection sees every page and auto-attaches to ones that appear later. That
matters more than it sounds: when the renderer dies, CEF comes back with fresh
target ids *and* a fresh browser UUID, so per-target connections would all go
stale at once.

The disconnect is the point. A dropped browser connection is precisely the
event we are hunting, so it is logged loudly rather than retried quietly.

Reading the output: entries appearing right after an `attached to …` line may
be CDP **replaying its buffer** rather than reporting something that just
happened. `Runtime.enable` hands over recent console entries and exceptions
from before we connected, which is useful after a reconnect — it means the
moments before a crash are not always lost — but it does mean the timestamp is
when the line was *recorded*.
"""

import argparse
import base64
import json
import os
import select
import socket
import ssl  # noqa: F401  (kept so a wss:// endpoint is a two-line change)
import struct
import sys
import time
import urllib.error
import urllib.request

DEFAULT_PORT = 8081
DEFAULT_LOG = os.path.expanduser("~/cef-watch/console.log")

#: Keep the evidence, drop the chatter. Steam's UI logs a *lot* at `log`/`info`,
#: and a full transcript would rotate the interesting part off disk long before
#: anyone went looking. `--all` turns this off.
NOISY_CONSOLE_TYPES = {"log", "info", "debug", "dir", "dirxml", "table", "trace",
                       "clear", "startGroup", "startGroupCollapsed", "endGroup",
                       "count", "timeEnd"}

#: Rotation. Three files at 4 MB is ~12 MB of history, which on a 60 s memory
#: cadence plus real errors is days — long enough that a crash overnight is
#: still on disk in the morning.
MAX_BYTES = 4 * 1024 * 1024
KEEP = 3

MEM_SAMPLE_SECONDS = 60


# --------------------------------------------------------------- logging


class Log:
    """Append-only log with size rotation, optionally teed to stdout."""

    def __init__(self, path, echo=False):
        self.path = path
        self.echo = echo
        self.handle = None
        # Consecutive-duplicate collapsing, the way syslog does it. Steam's own
        # UI emits the same warning ("MultiSourceImage created with no image
        # src") dozens of times in a burst; written out in full those would
        # rotate a real exception off disk within days. The 60 s memory sample
        # is always a different message, so a run can never stay unflushed for
        # longer than that.
        self._last = None
        self._repeat = 0
        if path:
            os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
            self._open()

    def _open(self):
        self.handle = open(self.path, "a", buffering=1, encoding="utf-8", errors="replace")

    def _rotate_if_needed(self):
        if not self.handle:
            return
        try:
            if self.handle.tell() < MAX_BYTES:
                return
        except OSError:
            return
        self.handle.close()
        for i in range(KEEP - 1, 0, -1):
            older, newer = f"{self.path}.{i + 1}", f"{self.path}.{i}"
            if os.path.exists(newer):
                os.replace(newer, older)
        os.replace(self.path, f"{self.path}.1")
        self._open()

    def write(self, level, message):
        key = (level, message)
        if key == self._last:
            self._repeat += 1
            return
        if self._repeat:
            count = self._repeat
            self._repeat = 0
            self._last = None
            self._emit("INFO", f"(previous message repeated {count} more time"
                               f"{'' if count == 1 else 's'})")
        self._last = key
        self._emit(level, message)

    def _emit(self, level, message):
        stamp = time.strftime("%Y-%m-%d %H:%M:%S")
        line = f"[{stamp}] {level:<5} {message}"
        if self.handle:
            self.handle.write(line + "\n")
            self._rotate_if_needed()
        if self.echo:
            print(line, flush=True)

    def info(self, m):
        self.write("INFO", m)

    def warn(self, m):
        self.write("WARN", m)

    def error(self, m):
        self.write("ERROR", m)

    def crash(self, m):
        # Deliberately shouty and deliberately a fixed string: this is what
        # someone greps for after the UI blinks out.
        self.write("CRASH", f"!!! {m}")


# --------------------------------------------------------- websocket client


class WebSocketError(Exception):
    pass


class WebSocket:
    """The ~5% of RFC 6455 a CDP client needs.

    Text frames in both directions, continuation frames reassembled, ping
    answered, close honoured. Client frames are masked because the spec
    requires it and CEF enforces it.
    """

    def __init__(self, url, timeout=10.0):
        if not url.startswith("ws://"):
            raise WebSocketError(f"only ws:// is supported here, got {url!r}")
        rest = url[len("ws://"):]
        hostport, _, path = rest.partition("/")
        path = "/" + path
        host, _, port = hostport.partition(":")
        port = int(port or 80)

        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.settimeout(timeout)
        self.buffer = bytearray()
        self._handshake(host, port, path)
        # Frames arrive in their own time; the read loop uses select() and
        # must not raise on an idle connection.
        self.sock.settimeout(None)

    def _handshake(self, host, port, path):
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        self.sock.sendall(request.encode())

        # Read exactly up to the end of the headers; anything past it is
        # already frame data and must be kept.
        while b"\r\n\r\n" not in self.buffer:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise WebSocketError("connection closed during handshake")
            self.buffer.extend(chunk)
        head, _, rest = bytes(self.buffer).partition(b"\r\n\r\n")
        self.buffer = bytearray(rest)
        status = head.split(b"\r\n", 1)[0].decode(errors="replace")
        if "101" not in status:
            raise WebSocketError(f"handshake refused: {status}")

    def send_text(self, text):
        payload = text.encode()
        header = bytearray([0x81])  # FIN + text
        length = len(payload)
        if length < 126:
            header.append(0x80 | length)
        elif length <= 0xFFFF:
            header.append(0x80 | 126)
            header.extend(struct.pack(">H", length))
        else:
            header.append(0x80 | 127)
            header.extend(struct.pack(">Q", length))
        mask = os.urandom(4)
        header.extend(mask)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(bytes(header) + masked)

    def _take_frame(self):
        """One complete frame from the buffer, or None if more bytes are needed."""
        buf = self.buffer
        if len(buf) < 2:
            return None
        fin = bool(buf[0] & 0x80)
        opcode = buf[0] & 0x0F
        masked = bool(buf[1] & 0x80)
        length = buf[1] & 0x7F
        offset = 2
        if length == 126:
            if len(buf) < offset + 2:
                return None
            length = struct.unpack(">H", buf[offset:offset + 2])[0]
            offset += 2
        elif length == 127:
            if len(buf) < offset + 8:
                return None
            length = struct.unpack(">Q", buf[offset:offset + 8])[0]
            offset += 8
        if masked:
            if len(buf) < offset + 4:
                return None
            mask = bytes(buf[offset:offset + 4])
            offset += 4
        if len(buf) < offset + length:
            return None
        payload = bytes(buf[offset:offset + length])
        if masked:
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        del buf[:offset + length]
        return fin, opcode, payload

    def pump(self):
        """Read whatever is available; yield complete text messages.

        Caller is expected to have established readability via select(), so a
        zero-length read here means the peer went away — which for our purposes
        means the renderer died.
        """
        chunk = self.sock.recv(65536)
        if not chunk:
            raise WebSocketError("connection closed by peer")
        self.buffer.extend(chunk)

        while True:
            frame = self._take_frame()
            if frame is None:
                return
            fin, opcode, payload = frame
            if opcode == 0x8:  # close
                raise WebSocketError("peer sent close")
            if opcode == 0x9:  # ping -> pong
                self._send_control(0xA, payload)
                continue
            if opcode == 0xA:  # pong
                continue
            if opcode in (0x0, 0x1, 0x2):
                self._fragments = getattr(self, "_fragments", bytearray())
                self._fragments.extend(payload)
                if fin:
                    message = bytes(self._fragments)
                    self._fragments = bytearray()
                    try:
                        yield message.decode(errors="replace")
                    except Exception:
                        pass

    def _send_control(self, opcode, payload):
        payload = payload[:125]
        mask = os.urandom(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(bytes([0x80 | opcode, 0x80 | len(payload)]) + mask + masked)

    def close(self):
        try:
            self._send_control(0x8, b"")
        except Exception:
            pass
        try:
            self.sock.close()
        except Exception:
            pass

    def fileno(self):
        return self.sock.fileno()


# ------------------------------------------------------------- CDP watching


def browser_ws_url(port, timeout=5.0):
    url = f"http://127.0.0.1:{port}/json/version"
    with urllib.request.urlopen(url, timeout=timeout) as response:
        data = json.load(response)
    ws = data.get("webSocketDebuggerUrl")
    if not ws:
        raise RuntimeError("no webSocketDebuggerUrl — is Steam running with CEF debugging?")
    return ws, data.get("Browser", "?")


def memory_line():
    """Available memory and swap, because the crash we chased had 11/14 GiB used.

    Cheap, and it turns "was the Deck under pressure?" from a question into a
    line in the same file as the exception.
    """
    try:
        values = {}
        with open("/proc/meminfo") as handle:
            for line in handle:
                key, _, rest = line.partition(":")
                values[key] = int(rest.split()[0])  # kB
    except OSError:
        return None
    total = values.get("MemTotal", 0) / 1048576
    available = values.get("MemAvailable", 0) / 1048576
    swap_total = values.get("SwapTotal", 0) / 1048576
    swap_free = values.get("SwapFree", 0) / 1048576
    return (f"mem avail {available:.1f}/{total:.1f} GiB, "
            f"swap used {swap_total - swap_free:.1f}/{swap_total:.1f} GiB")


def describe_args(args):
    """Flatten console arguments into something greppable on one line."""
    out = []
    for arg in args or []:
        if "value" in arg:
            out.append(str(arg["value"]))
        elif arg.get("description"):
            out.append(arg["description"])
        elif arg.get("preview", {}).get("properties"):
            props = arg["preview"]["properties"]
            inner = ", ".join(f"{p.get('name')}: {p.get('value')}" for p in props[:8])
            out.append(f"{arg.get('className', 'Object')}{{{inner}}}")
        else:
            out.append(arg.get("type", "?"))
    return " ".join(out)


def format_stack(trace):
    if not trace:
        return ""
    frames = trace.get("callFrames") or []
    parts = []
    for frame in frames[:12]:
        name = frame.get("functionName") or "(anonymous)"
        url = frame.get("url") or ""
        parts.append(f"    at {name} ({url}:{frame.get('lineNumber')}:{frame.get('columnNumber')})")
    return "\n".join(parts)


class Watcher:
    def __init__(self, log, port, include_all=False):
        self.log = log
        self.port = port
        self.include_all = include_all
        self.next_id = 1
        self.sessions = {}   # sessionId -> target title
        self.targets = {}    # targetId  -> title
        self.attached = set()  # targetIds we already hold a session on
        self.ws = None

    def send(self, method, params=None, session=None):
        message = {"id": self.next_id, "method": method, "params": params or {}}
        if session:
            message["sessionId"] = session
        self.next_id += 1
        self.ws.send_text(json.dumps(message))

    def label(self, session):
        return self.sessions.get(session, session or "browser")

    def handle(self, message):
        method = message.get("method")
        params = message.get("params") or {}
        session = message.get("sessionId")

        if method == "Target.targetCreated" or method == "Target.targetInfoChanged":
            info = params.get("targetInfo", {})
            if info.get("type") == "page":
                target_id = info["targetId"]
                self.targets[target_id] = info.get("title", "?")
                # Attach even when `attached` is already true. Decky holds a
                # session on SharedJSContext — which is exactly where plugin
                # code runs and where a plugin exception would surface — and
                # CDP allows several flattened sessions on one target, so
                # skipping the attached ones would blind us to the one page
                # that matters most.
                if target_id not in self.attached:
                    self.attached.add(target_id)
                    self.send("Target.attachToTarget",
                              {"targetId": target_id, "flatten": True})
            return

        if method == "Target.attachedToTarget":
            info = params.get("targetInfo", {})
            sid = params.get("sessionId")
            title = info.get("title", "?")
            self.sessions[sid] = title
            self.log.info(f"attached to {title}")
            # Runtime gives exceptions and console; Log gives the browser's own
            # entries (network, security) that never reach console.
            self.send("Runtime.enable", session=sid)
            self.send("Log.enable", session=sid)
            return

        if method == "Target.detachedFromTarget":
            sid = params.get("sessionId")
            self.log.info(f"detached from {self.label(sid)}")
            self.sessions.pop(sid, None)
            target_id = params.get("targetId")
            if target_id:
                # Let it be re-attached if it comes back under the same id.
                self.attached.discard(target_id)
            return

        if method == "Target.targetDestroyed":
            target_id = params.get("targetId")
            if target_id in self.targets:
                self.log.info(f"target gone: {self.targets.pop(target_id)}")
            self.attached.discard(target_id)
            return

        if method in ("Target.targetCrashed", "Inspector.targetCrashed"):
            # The renderer died and told us. Rarer than a hard segfault (where
            # the socket simply drops) but unambiguous when it happens.
            self.log.crash(f"targetCrashed {self.label(session)} {json.dumps(params)}")
            return

        if method == "Runtime.exceptionThrown":
            details = params.get("exceptionDetails", {})
            text = details.get("text", "")
            exception = details.get("exception") or {}
            description = exception.get("description") or exception.get("value") or ""
            where = f"{details.get('url', '')}:{details.get('lineNumber')}:{details.get('columnNumber')}"
            self.log.error(f"[{self.label(session)}] EXCEPTION {text} {description} @ {where}")
            stack = format_stack(details.get("stackTrace"))
            if stack:
                self.log.error(f"[{self.label(session)}] stack:\n{stack}")
            return

        if method == "Runtime.consoleAPICalled":
            kind = params.get("type", "log")
            if not self.include_all and kind in NOISY_CONSOLE_TYPES:
                return
            body = describe_args(params.get("args"))
            self.log.write("JS", f"[{self.label(session)}] console.{kind}: {body}")
            if kind in ("error", "assert"):
                stack = format_stack(params.get("stackTrace"))
                if stack:
                    self.log.write("JS", f"[{self.label(session)}] stack:\n{stack}")
            return

        if method == "Log.entryAdded":
            entry = params.get("entry", {})
            level = entry.get("level", "info")
            if not self.include_all and level not in ("error", "warning"):
                return
            # Steam's library art 404s constantly against steamloopback.host.
            # Left in, these outnumber real events by orders of magnitude and
            # would rotate the evidence off disk. `--all` keeps them.
            if not self.include_all and entry.get("source") == "network":
                return
            self.log.write("JS", f"[{self.label(session)}] log.{level} ({entry.get('source')}): "
                                 f"{entry.get('text')} {entry.get('url') or ''}")
            return

    def run_once(self):
        """One browser connection, until it drops. Returns on disconnect."""
        ws_url, browser = browser_ws_url(self.port)
        self.ws = WebSocket(ws_url)
        self.sessions.clear()
        self.targets.clear()
        self.log.info(f"connected to {browser} at {ws_url}")
        mem = memory_line()
        if mem:
            self.log.info(mem)

        # Discovery drives everything else: every existing page arrives as a
        # targetCreated, and so does every page created later.
        self.send("Target.setDiscoverTargets", {"discover": True})

        last_sample = time.time()
        while True:
            readable, _, _ = select.select([self.ws], [], [], 5.0)
            now = time.time()
            if now - last_sample >= MEM_SAMPLE_SECONDS:
                last_sample = now
                mem = memory_line()
                if mem:
                    self.log.info(mem)
            if not readable:
                continue
            for raw in self.ws.pump():
                try:
                    message = json.loads(raw)
                except ValueError:
                    continue
                if "method" in message:
                    try:
                        self.handle(message)
                    except Exception as e:  # never let one odd event kill the watch
                        self.log.warn(f"handler error on {message.get('method')}: {e}")

    def run_forever(self):
        backoff = 1.0
        connected_at = None
        while True:
            try:
                connected_at = time.time()
                self.run_once()
            except (WebSocketError, OSError, urllib.error.URLError, RuntimeError) as e:
                lasted = time.time() - (connected_at or time.time())
                if lasted > 10:
                    # We were watching a live browser and it went away. That is
                    # the steamwebhelper crash this whole tool exists for.
                    self.log.crash(f"CEF CONNECTION LOST after {lasted:.0f}s — "
                                   f"steamwebhelper likely died ({e})")
                    mem = memory_line()
                    if mem:
                        self.log.crash(f"at loss: {mem}")
                    backoff = 1.0
                else:
                    self.log.warn(f"not connected ({e}); retrying in {backoff:.0f}s")
            except KeyboardInterrupt:
                self.log.info("stopping")
                return
            finally:
                if self.ws:
                    self.ws.close()
                    self.ws = None
            time.sleep(backoff)
            backoff = min(backoff * 2, 30.0)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--log", default=DEFAULT_LOG, help="log file (rotated)")
    parser.add_argument("--stdout", action="store_true", help="also print to stdout")
    parser.add_argument("--all", action="store_true",
                        help="record every console call, not just warnings and errors")
    parser.add_argument("--once", action="store_true",
                        help="one connection, echo to stdout, exit when it drops")
    args = parser.parse_args()

    log = Log(None if args.once and args.log == DEFAULT_LOG else args.log,
              echo=args.stdout or args.once)
    watcher = Watcher(log, args.port, include_all=args.all)

    if args.once:
        try:
            watcher.run_once()
        except KeyboardInterrupt:
            pass
        except Exception as e:
            log.crash(f"connection ended: {e}")
            return 1
        return 0

    log.info("cef-watch starting")
    watcher.run_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())

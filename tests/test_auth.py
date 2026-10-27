"""Offline checks for the credential lifecycle and authenticated Decky proxy.

Run with `python3 tests/test_auth.py`. No Deck, Wine, or third-party modules.
"""

import asyncio
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import threading
import types
import unittest
from unittest.mock import AsyncMock, Mock, patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "py_modules"))
sys.path.insert(0, str(ROOT))

decky = types.ModuleType("decky")
decky.DECKY_USER_HOME = tempfile.gettempdir()
decky.logger = Mock()
sys.modules["decky"] = decky

import trainer_auth  # noqa: E402
import main  # noqa: E402


class SecretTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name) / "settings"
        self.path = self.directory / trainer_auth.SECRET_FILENAME

    def test_creation_permissions_and_persistence(self):
        with patch("trainer_auth.secrets.token_hex", return_value="a" * 64) as random:
            first = trainer_auth.load_secret(str(self.directory))
            second = trainer_auth.load_secret(str(self.directory))
        self.assertEqual(first, "a" * 64)
        self.assertEqual(first, second)
        random.assert_called_once_with(32)
        self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o600)
        self.assertEqual(list(self.directory.iterdir()), [self.path])

    def test_existing_file_permissions_are_tightened(self):
        secret = trainer_auth.load_secret(str(self.directory))
        self.path.chmod(0o644)
        self.assertEqual(trainer_auth.load_secret(str(self.directory)), secret)
        self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o600)

    def test_concurrent_starts_all_use_the_same_secret(self):
        with ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(lambda _: trainer_auth.load_secret(str(self.directory)), range(16)))
        self.assertEqual(len(set(results)), 1)
        self.assertEqual(list(self.directory.iterdir()), [self.path])

    def test_invalid_files_are_never_silently_replaced(self):
        self.directory.mkdir()
        for value in [b"", b"short", b"G" * 64, b"a" * 64 + b"\ninjected", b"\xff" * 64]:
            self.path.write_bytes(value)
            with self.subTest(value=value), self.assertRaises(ValueError):
                trainer_auth.load_secret(str(self.directory))
            self.assertEqual(self.path.read_bytes(), value)

    def test_symlinks_and_nonregular_files_are_rejected(self):
        self.directory.mkdir()
        target = self.directory / "target"
        target.write_text("a" * 64)
        self.path.symlink_to(target)
        with self.assertRaises(OSError):
            trainer_auth.load_secret(str(self.directory))
        self.path.unlink()
        os.mkfifo(self.path)
        with self.assertRaises(ValueError):
            trainer_auth.load_secret(str(self.directory))

    def test_write_failure_leaves_no_partial_credential(self):
        with patch("trainer_auth.os.fsync", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                trainer_auth.load_secret(str(self.directory))
        self.assertEqual(list(self.directory.iterdir()), [])


class PluginAuthTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.settings_dir = os.path.join(self.temp.name, "settings")
        decky.DECKY_PLUGIN_SETTINGS_DIR = self.settings_dir
        decky.DECKY_PLUGIN_LOG_DIR = self.temp.name
        self.plugin = main.Plugin()
        await self.plugin._main()

    async def test_reload_reuses_secret_and_settings_do_not_expose_it(self):
        other = main.Plugin()
        await other._main()
        self.assertEqual(self.plugin._auth_secret, other._auth_secret)
        self.assertNotIn(self.plugin._auth_secret, json.dumps(await other.get_settings()))
        self.assertNotIn(self.plugin._auth_secret, str(decky.logger.mock_calls))

    async def test_all_probes_and_proxy_calls_authenticate(self):
        with patch.object(main, "http_json", return_value=(200, {"process": {}})) as transport:
            self.plugin.trainer_name = "test.exe"  # No unrelated /proc adoption.
            self.plugin._trainer_procs = AsyncMock(return_value=set())
            self.assertTrue(await self.plugin._api_up())
            self.assertTrue((await self.plugin.detect_trainer())["apiUp"])
            self.assertTrue((await self.plugin.get_trainer_status())["apiUp"])
            self.assertTrue((await self.plugin.api_request("POST", "/api/features/toggle", {"id": 1}))["ok"])
            self.plugin._trainer_procs = AsyncMock(side_effect=[{"123"}, set()])
            self.assertTrue((await self.plugin.stop_trainer())["ok"])
        self.assertGreaterEqual(transport.call_count, 6)
        for call in transport.call_args_list:
            self.assertEqual(call.kwargs["auth_secret"], self.plugin._auth_secret)
            self.assertTrue(call.args[1].startswith("http://127.0.0.1:"))
        self.assertIn("/api/shutdown", transport.call_args_list[-1].args[1])

    async def test_unauthorized_responses_are_not_a_healthy_trainer(self):
        with patch.object(main, "http_json", return_value=(401, {"error": "Unauthorized"})):
            self.assertFalse(await self.plugin._api_up())
            self.assertFalse((await self.plugin.detect_trainer())["apiUp"])
            result = await self.plugin.api_request("GET", "/api/features")
            self.assertFalse(result["ok"])
            self.assertEqual(result["status"], 401)

    async def test_redirects_are_reported_as_failed_actions(self):
        with patch.object(main, "http_json", return_value=(302, None)):
            result = await self.plugin.api_request("POST", "/api/features/toggle", {"id": 1})
        self.assertFalse(result["ok"])
        self.assertEqual(result["status"], 302)

    async def test_launch_passes_secret_and_selected_port_to_child(self):
        trainer = Path(self.temp.name) / "test.exe"
        trainer.write_bytes(b"MZ")
        game_env = {"WINEPREFIX": "/game/pfx", trainer_auth.SECRET_ENV: "untrusted"}
        self.plugin.settings["port"] = 42345
        self.plugin._api_status = AsyncMock(return_value=None)
        self.plugin._wait_for_api = AsyncMock(return_value=True)
        with patch.object(main, "find_game_process", return_value=("123", "game.exe", game_env)), \
                patch.object(main, "resolve_wine", return_value="/proton/wine"), \
                patch.object(main, "_port_is_listening", return_value=False), \
                patch.object(main.os, "geteuid", return_value=1000), \
                patch.object(main, "_spawn", return_value=Mock()) as spawn:
            result = await self.plugin.launch_trainer("10", str(trainer))
        self.assertTrue(result["ok"])
        self.assertEqual(spawn.call_args.args[2][trainer_auth.SECRET_ENV], self.plugin._auth_secret)
        self.assertEqual(spawn.call_args.args[-1], 42345)
        self.assertEqual(game_env[trainer_auth.SECRET_ENV], "untrusted")

    async def test_spawn_keeps_secret_out_of_command_line(self):
        env = {trainer_auth.SECRET_ENV: self.plugin._auth_secret}
        with patch.object(main.subprocess, "Popen") as popen:
            main._spawn("/wine", "/trainer.exe", env, self.temp.name,
                        os.path.join(self.temp.name, "trainer.log"), 42345)
        self.assertEqual(popen.call_args.args[0], ["/wine", "/trainer.exe", "--port", "42345"])
        self.assertEqual(popen.call_args.kwargs["env"], env)


class TransportTests(unittest.TestCase):
    def setUp(self):
        self.calls = []
        calls = self.calls

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                calls.append((self.path, self.headers.get("Authorization")))
                if self.path == "/redirect":
                    self.send_response(302)
                    self.send_header("Location", "/leaked")
                    self.end_headers()
                else:
                    self.send_response(self.server.response_status)
                    self.end_headers()
                    self.wfile.write(b'{"ok":true}')

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.response_status = 200
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.close_server)
        self.base = f"http://127.0.0.1:{self.server.server_port}"

    def close_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def test_header_is_sent_and_environment_proxy_is_ignored(self):
        with patch.dict(os.environ, {"http_proxy": "http://127.0.0.1:1", "no_proxy": ""}):
            status, payload = main.http_json("GET", self.base + "/api/features", auth_secret="a" * 64)
        self.assertEqual(status, 200)
        self.assertTrue(payload["ok"])
        self.assertEqual(self.calls, [("/api/features", "Bearer " + "a" * 64)])

    def test_redirects_are_not_followed_with_the_credential(self):
        status, _ = main.http_json("GET", self.base + "/redirect", auth_secret="a" * 64)
        self.assertEqual(status, 302)
        self.assertEqual(len(self.calls), 1)

    def test_launch_rejects_authentication_failure_without_spawning(self):
        plugin = main.Plugin()
        plugin.settings = {"port": self.server.server_port}
        plugin._auth_secret = "a" * 64
        with tempfile.TemporaryDirectory() as directory:
            trainer = Path(directory) / "test.exe"
            trainer.write_bytes(b"MZ")
            with patch.object(main, "_spawn") as spawn, \
                    patch.object(main, "find_game_process", side_effect=AssertionError("must not scan games")), \
                    patch.object(plugin, "_wait_for_api", new_callable=AsyncMock) as wait:
                for status in [401, 403]:
                    with self.subTest(status=status):
                        self.server.response_status = status
                        result = asyncio.run(plugin.launch_trainer("10", str(trainer)))
                        self.assertFalse(result["ok"])
                        self.assertIn("authentication", result["error"])
                        self.assertIn(str(self.server.server_port), result["error"])
                        self.assertNotIn(plugin._auth_secret, result["error"])
                spawn.assert_not_called()
                wait.assert_not_awaited()

    def test_launch_rejects_an_occupied_port_even_if_http_is_unhealthy(self):
        plugin = main.Plugin()
        plugin.settings = {"port": self.server.server_port}
        plugin._auth_secret = "a" * 64
        with patch.object(main, "_spawn") as spawn, \
                patch.object(main, "find_game_process", side_effect=AssertionError("must not scan games")):
            self.server.response_status = 503
            result = asyncio.run(plugin.launch_trainer("10", "/unused.exe"))
            self.assertFalse(result["ok"])
            self.assertIn("port", result["error"])
            self.assertIn("503", result["error"])
            spawn.assert_not_called()

    def test_launch_rejects_an_occupied_port_without_an_http_response(self):
        plugin = main.Plugin()
        plugin.settings = {"port": self.server.server_port}
        plugin._auth_secret = "a" * 64
        with patch.object(main, "http_json", side_effect=TimeoutError), \
                patch.object(main, "_spawn") as spawn, \
                patch.object(main, "find_game_process", side_effect=AssertionError("must not scan games")):
            result = asyncio.run(plugin.launch_trainer("10", "/unused.exe"))
            self.assertFalse(result["ok"])
            self.assertIn("port", result["error"])
            self.assertIn("in use", result["error"])
            spawn.assert_not_called()


if __name__ == "__main__":
    unittest.main()

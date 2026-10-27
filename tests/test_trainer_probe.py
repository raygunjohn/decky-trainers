"""Offline regressions for shared trainer probes and busy status responses.

Run with `python3 tests/test_trainer_probe.py`. No Deck or Wine is required.
"""

import asyncio
import json
from pathlib import Path
import sys
import tempfile
import threading
import types
import unittest
from unittest.mock import AsyncMock, Mock, patch


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "py_modules"))
sys.path.insert(0, str(ROOT))

decky = sys.modules.setdefault("decky", types.ModuleType("decky"))
decky.DECKY_USER_HOME = tempfile.gettempdir()
decky.logger = Mock()

import main  # noqa: E402


class TrainerProbeTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.settings_dir = Path(self.temp.name) / "settings"
        self.settings_dir.mkdir()
        (self.settings_dir / "settings.json").write_text(json.dumps({
            "trainerDir": str(Path(self.temp.name) / "trainers"),
        }))
        main.decky.DECKY_PLUGIN_SETTINGS_DIR = str(self.settings_dir)
        main.decky.DECKY_PLUGIN_LOG_DIR = self.temp.name
        self.plugin = main.Plugin()
        await self.plugin._main()
        self.plugin.trainer_name = "fixture.exe"
        self.plugin._trainer_procs = AsyncMock(return_value=set())
        self.plugin._adopt_from_socket = Mock(return_value=False)
        self.plugin._installed_process = Mock(return_value="")

    @staticmethod
    def features(attached=True):
        return {
            "trainer": {"name": "fixture.exe"},
            "process": {"attached": attached, "processName": "game.exe"},
            "features": [],
        }

    async def test_probe_preserves_http_payload_and_uses_selected_timeout(self):
        payload = self.features()
        with patch.object(self.plugin, "_trainer_request", return_value=(200, payload)) as request, \
                patch.object(main, "_port_is_listening") as listening:
            probe = await self.plugin._probe_trainer(timeout=0.25)
        self.assertTrue(probe.reachable)
        self.assertTrue(probe.authenticated)
        self.assertEqual(probe.status, 200)
        self.assertIs(probe.payload, payload)
        self.assertFalse(probe.busy)
        self.assertIsNone(probe.error)
        request.assert_called_once_with("GET", "/api/features", None, 0.25)
        listening.assert_not_called()

    async def test_busy_response_is_reachable_and_authenticated(self):
        payload = {"busy": True, "features": []}
        with patch.object(self.plugin, "_trainer_request", return_value=(200, payload)) as request:
            probe = await self.plugin._probe_trainer()
        self.assertTrue(probe.reachable)
        self.assertTrue(probe.authenticated)
        self.assertTrue(probe.busy)
        self.assertIs(probe.payload, payload)
        self.assertEqual(request.call_count, 1)

    async def test_http_rejections_remain_reachable_without_authentication(self):
        for status in (401, 403, 503):
            with self.subTest(status=status), \
                    patch.object(self.plugin, "_trainer_request", return_value=(status, {"error": "rejected"})), \
                    patch.object(main, "_port_is_listening") as listening:
                probe = await self.plugin._probe_trainer()
            self.assertTrue(probe.reachable)
            self.assertFalse(probe.authenticated)
            self.assertEqual(probe.status, status)
            self.assertFalse(probe.busy)
            self.assertTrue(probe.error)
            self.assertNotIn(self.plugin._auth_secret, probe.error)
            listening.assert_not_called()

    async def test_timed_out_http_keeps_an_occupied_socket_reachable(self):
        self.plugin.settings["port"] = 42179
        with patch.object(self.plugin, "_trainer_request", side_effect=TimeoutError("timed out")) as request, \
                patch.object(main, "_port_is_listening", return_value=True) as listening:
            probe = await self.plugin._probe_trainer(timeout=0.25)
        self.assertTrue(probe.reachable)
        self.assertFalse(probe.authenticated)
        self.assertIsNone(probe.status)
        self.assertIsNone(probe.payload)
        self.assertFalse(probe.busy)
        request.assert_called_once_with("GET", "/api/features", None, 0.25)
        listening.assert_called_once_with(42179)

    async def test_connection_failure_without_listener_is_unavailable(self):
        with patch.object(self.plugin, "_trainer_request", side_effect=ConnectionRefusedError()), \
                patch.object(main, "_port_is_listening", return_value=False) as listening:
            probe = await self.plugin._probe_trainer()
        self.assertFalse(probe.reachable)
        self.assertFalse(probe.authenticated)
        self.assertIsNone(probe.status)
        self.assertIsNone(probe.payload)
        self.assertFalse(probe.busy)
        listening.assert_called_once_with(self.plugin._port())

    async def test_api_up_uses_authenticated_state_from_shared_probe(self):
        for authenticated in (False, True):
            with self.subTest(authenticated=authenticated):
                self.plugin._probe_trainer = AsyncMock(return_value=main.TrainerProbe(
                    reachable=True, authenticated=authenticated,
                ))
                self.assertIs(await self.plugin._api_up(timeout=0.25), authenticated)
                self.plugin._probe_trainer.assert_awaited_once_with(timeout=0.25)

    async def test_status_fetches_features_once(self):
        with patch.object(self.plugin, "_trainer_request", return_value=(200, self.features())) as request:
            status = await self.plugin.get_trainer_status()
        self.assertEqual(request.call_count, 1)
        self.assertEqual(request.call_args.args[:2], ("GET", "/api/features"))
        self.assertTrue(status["running"])
        self.assertTrue(status["apiUp"])
        self.assertTrue(status["reachable"])
        self.assertTrue(status["authenticated"])
        self.assertFalse(status["busy"])
        self.assertIs(status["attached"], True)
        self.assertEqual(status["gameExe"], "game.exe")

    async def test_detection_fetches_features_once(self):
        with patch.object(self.plugin, "_trainer_request", return_value=(200, self.features())) as request:
            result = await self.plugin.detect_trainer()
        self.assertEqual(request.call_count, 1)
        self.assertTrue(result["apiUp"])
        self.assertTrue(result["reachable"])
        self.assertTrue(result["authenticated"])
        self.assertFalse(result["busy"])
        self.assertIs(result["attached"], True)
        self.assertEqual(result["gameExe"], "game.exe")

    async def test_detection_and_status_share_confirmed_attachment_during_busy(self):
        responses = [
            (200, self.features()),
            (200, {"busy": True, "features": []}),
            (200, {"busy": True, "features": []}),
        ]
        with patch.object(self.plugin, "_trainer_request", side_effect=responses) as request:
            detected = await self.plugin.detect_trainer()
            status = await self.plugin.get_trainer_status()
            detected_busy = await self.plugin.detect_trainer()
        self.assertIs(detected["attached"], True)
        for result in (status, detected_busy):
            self.assertTrue(result["apiUp"])
            self.assertTrue(result["busy"])
            self.assertIs(result["attached"], True)
            self.assertEqual(result["gameExe"], "game.exe")
        self.assertEqual(request.call_count, 3)

    async def test_status_attachment_is_reused_by_busy_detection(self):
        with patch.object(self.plugin, "_trainer_request", side_effect=[
            (200, self.features()),
            (200, {"busy": True, "features": []}),
        ]):
            status = await self.plugin.get_trainer_status()
            detected = await self.plugin.detect_trainer()
        self.assertIs(status["attached"], True)
        self.assertIs(detected["attached"], True)
        self.assertTrue(detected["busy"])

    async def test_first_busy_response_does_not_claim_detachment(self):
        with patch.object(self.plugin, "_trainer_request", return_value=(200, {"busy": True, "features": []})) as request:
            detected = await self.plugin.detect_trainer()
            status = await self.plugin.get_trainer_status()
        for result in (detected, status):
            self.assertTrue(result["apiUp"])
            self.assertTrue(result["busy"])
            self.assertIsNone(result["attached"])
        self.assertEqual(request.call_count, 2)

    async def test_explicit_detachment_replaces_cached_attachment(self):
        with patch.object(self.plugin, "_trainer_request", side_effect=[
            (200, self.features()),
            (200, self.features(attached=False)),
            (200, {"busy": True, "features": []}),
        ]):
            self.assertIs((await self.plugin.get_trainer_status())["attached"], True)
            self.assertIs((await self.plugin.detect_trainer())["attached"], False)
            self.assertIs((await self.plugin.get_trainer_status())["attached"], False)

    async def test_port_change_discards_previous_attachment(self):
        with patch.object(self.plugin, "_trainer_request", side_effect=[
            (200, self.features()),
            (200, {"busy": True, "features": []}),
        ]):
            self.assertIs((await self.plugin.detect_trainer())["attached"], True)
            self.plugin.settings["port"] += 1
            status = await self.plugin.get_trainer_status()
        self.assertTrue(status["busy"])
        self.assertIsNone(status["attached"])

    async def test_inflight_response_from_previous_port_is_discarded(self):
        started = threading.Event()
        release = threading.Event()
        calls = 0

        def request(*args):
            nonlocal calls
            calls += 1
            if calls == 1:
                started.set()
                if not release.wait(timeout=3):
                    raise TimeoutError("test did not release the previous-port response")
                return 200, self.features()
            return 200, {"busy": True, "features": []}

        with patch.object(self.plugin, "_trainer_request", side_effect=request) as transport:
            pending = asyncio.create_task(self.plugin.get_trainer_status())
            try:
                self.assertTrue(await asyncio.to_thread(started.wait, 3))
                self.plugin.settings["port"] += 1
                release.set()
                status = await asyncio.wait_for(pending, timeout=3)
                detected = await self.plugin.detect_trainer()
            finally:
                release.set()
                if not pending.done():
                    await asyncio.wait_for(pending, timeout=3)
        for result in (status, detected):
            self.assertTrue(result["busy"])
            self.assertIsNone(result["attached"])
        self.assertEqual(transport.call_count, 3)

    async def test_reset_discards_previous_attachment(self):
        with patch.object(self.plugin, "_trainer_request", side_effect=[
            (200, self.features()),
            (200, {"busy": True, "features": []}),
        ]):
            self.assertIs((await self.plugin.get_trainer_status())["attached"], True)
            self.plugin._reset_trainer_state()
            detected = await self.plugin.detect_trainer()
        self.assertTrue(detected["busy"])
        self.assertIsNone(detected["attached"])

    async def test_unreachable_trainer_discards_previous_attachment(self):
        with patch.object(self.plugin, "_trainer_request", side_effect=[
            (200, self.features()),
            ConnectionRefusedError(),
            (200, {"busy": True, "features": []}),
        ]), patch.object(main, "_port_is_listening", return_value=False):
            self.assertIs((await self.plugin.get_trainer_status())["attached"], True)
            unavailable = await self.plugin.get_trainer_status()
            restarted = await self.plugin.detect_trainer()
        self.assertFalse(unavailable["reachable"])
        self.assertIsNone(unavailable["attached"])
        self.assertTrue(restarted["busy"])
        self.assertIsNone(restarted["attached"])

    async def test_authentication_failure_has_distinct_status_diagnostics(self):
        with patch.object(self.plugin, "_trainer_request", return_value=(401, {"error": "Unauthorized"})) as request:
            detected = await self.plugin.detect_trainer()
            status = await self.plugin.get_trainer_status()
        for result in (detected, status):
            self.assertFalse(result["apiUp"])
            self.assertTrue(result["reachable"])
            self.assertFalse(result["authenticated"])
            self.assertFalse(result["busy"])
        self.assertEqual(request.call_count, 2)

    async def test_launch_rejects_occupied_unauthenticated_port_after_one_probe(self):
        with patch.object(self.plugin, "_trainer_request", return_value=(401, {"error": "Unauthorized"})) as request, \
                patch.object(main, "find_game_process") as find_game, \
                patch.object(main, "_spawn") as spawn, \
                patch.object(main, "_port_is_listening") as listening:
            result = await self.plugin.launch_trainer("10", "/unused.exe")
        self.assertFalse(result["ok"])
        self.assertIn("authentication", result["error"])
        self.assertEqual(request.call_count, 1)
        find_game.assert_not_called()
        spawn.assert_not_called()
        listening.assert_not_called()


if __name__ == "__main__":
    unittest.main()

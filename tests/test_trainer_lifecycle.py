"""Offline regression checks for confirmed trainer shutdown.

Run with `python3 tests/test_trainer_lifecycle.py`. All process, signal, and
listener operations are mocked; these tests never touch a running trainer.
"""

from contextlib import ExitStack
from pathlib import Path
import signal
import sys
import tempfile
import types
import unittest
from unittest.mock import AsyncMock, Mock, call, patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "py_modules"))
sys.path.insert(0, str(ROOT))

if "decky" not in sys.modules:
    decky = types.ModuleType("decky")
    decky.DECKY_USER_HOME = tempfile.gettempdir()
    decky.logger = Mock()
    sys.modules["decky"] = decky

import main  # noqa: E402


class ShutdownTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.plugin = main.Plugin()
        self.plugin.settings = dict(main.DEFAULT_SETTINGS)
        self.plugin._auth_secret = "a" * 64
        self.plugin.proc = Mock(pid=4321)
        self.plugin.trainer_name = "RDR2 trainer.exe"
        self.plugin.trainer_path = "/trainers/RDR2 trainer.exe"
        self.plugin.prefix_path = "/games/rdr2/pfx"
        self.plugin.game_exe = "RDR2.exe"
        self.plugin.adopted = True
        self.plugin.sidecar_process = "RDR2.exe"
        self.tracking = {
            field: getattr(self.plugin, field)
            for field in (
                "proc", "trainer_name", "trainer_path", "prefix_path", "game_exe",
                "adopted", "sidecar_process",
            )
        }
        self.alive = {"4321"}
        self.listening = True
        self.shutdown_reply = (401, {"error": "Unauthorized"})

        stack = self.enterContext(ExitStack())
        self.processes = stack.enter_context(patch.object(
            self.plugin, "_trainer_procs", new=AsyncMock(side_effect=lambda: set(self.alive)),
        ))
        self.probe = stack.enter_context(patch.object(
            self.plugin, "_probe_trainer", new=AsyncMock(side_effect=self._probe),
        ))
        self.request = stack.enter_context(patch.object(
            self.plugin, "_trainer_request", new=Mock(side_effect=lambda *_: self.shutdown_reply),
        ))
        self.kill = stack.enter_context(patch.object(main.os, "kill", new=Mock()))
        stack.enter_context(patch.object(main, "process_alive", side_effect=lambda pid: str(pid) in self.alive))
        stack.enter_context(patch.object(main, "_port_is_listening", side_effect=lambda _: self.listening))
        stack.enter_context(patch.object(main.asyncio, "sleep", new=AsyncMock()))
        stack.enter_context(patch.object(main, "SHUTDOWN_GRACE_TICKS", 2))
        stack.enter_context(patch.object(main, "SHUTDOWN_TICK", 0.0))

    async def _probe(self, timeout=2.0):
        return main.TrainerProbe(
            reachable=self.listening,
            authenticated=False,
            status=401 if self.listening else None,
        )

    def assert_tracking_preserved(self):
        self.assertEqual(
            {field: getattr(self.plugin, field) for field in self.tracking},
            self.tracking,
        )

    def assert_tracking_cleared(self):
        self.assertIsNone(self.plugin.proc)
        self.assertEqual(self.plugin.trainer_name, "")
        self.assertEqual(self.plugin.trainer_path, "")
        self.assertEqual(self.plugin.prefix_path, "")
        self.assertEqual(self.plugin.game_exe, "")
        self.assertFalse(self.plugin.adopted)
        self.assertEqual(self.plugin.sidecar_process, "")

    async def test_unauthorized_shutdown_and_denied_signals_preserve_live_trainer(self):
        self.kill.side_effect = PermissionError("not permitted")

        result = await self.plugin.stop_trainer()

        self.assertFalse(result["ok"])
        self.assertEqual(result["killed"], 0)
        self.assertFalse(result["apiUp"])
        self.assertIsNone(result["unhooked"])
        self.assertTrue(result["error"])
        self.assert_tracking_preserved()
        self.assertEqual(self.kill.call_args_list, [
            call(4321, signal.SIGTERM),
            call(4321, signal.SIGKILL),
        ])

    async def test_sigterm_exit_is_confirmed_before_clearing_tracking(self):
        def exit_on_signal(pid, sig):
            self.assertEqual(sig, signal.SIGTERM)
            self.alive.remove(str(pid))
            self.listening = False

        self.kill.side_effect = exit_on_signal

        result = await self.plugin.stop_trainer()

        self.assertTrue(result["ok"])
        self.assertEqual(result["killed"], 1)
        self.assertFalse(result["apiUp"])
        self.kill.assert_called_once_with(4321, signal.SIGTERM)
        self.assert_tracking_cleared()

    async def test_sigkill_delivery_does_not_confirm_process_exit(self):
        # Both signals can return successfully while the process remains in
        # /proc, for example while it is stuck in an uninterruptible syscall.
        result = await self.plugin.stop_trainer()

        self.assertFalse(result["ok"])
        self.assertEqual(result["killed"], 0)
        self.assertTrue(result["error"])
        self.assertEqual(self.kill.call_count, 2)
        self.assert_tracking_preserved()

    async def test_pid_disappearing_from_scan_is_still_checked_for_exit(self):
        # Closing the listener and dropping out of name discovery does not
        # establish that a previously identified trainer PID has exited.
        self.listening = False
        scans = iter([{"4321"}])
        self.processes.side_effect = lambda: next(scans, set())
        self.kill.side_effect = PermissionError("not permitted")

        result = await self.plugin.stop_trainer()

        self.assertFalse(result["ok"])
        self.assertEqual(result["killed"], 0)
        self.assertTrue(result["error"])
        self.assertEqual(self.kill.call_count, 2)
        self.assert_tracking_preserved()

    async def test_retry_retains_live_pid_lost_from_discovery_after_failed_stop(self):
        # Adopted trainers have no Popen handle. Once a PID closes its socket
        # and drops out of the name scan, a later stop must still remember it.
        self.plugin.proc = None
        self.tracking["proc"] = None
        self.listening = False
        scans = iter([{"4321"}])
        self.processes.side_effect = lambda: next(scans, set())
        self.kill.side_effect = PermissionError("not permitted")

        for attempt in range(2):
            with self.subTest(attempt=attempt):
                result = await self.plugin.stop_trainer()
                self.assertFalse(result["ok"])
                self.assertEqual(result["killed"], 0)
                self.assertTrue(result["error"])
                self.assert_tracking_preserved()

        self.assertEqual(self.kill.call_count, 4)

    async def test_launch_does_not_replace_surviving_trainer_after_failed_stop(self):
        self.plugin.proc = None
        self.tracking["proc"] = None
        self.listening = False
        scans = iter([{"4321"}])
        self.processes.side_effect = lambda: next(scans, set())
        self.kill.side_effect = PermissionError("not permitted")
        stopped = await self.plugin.stop_trainer()
        self.assertFalse(stopped["ok"])

        with patch.object(main.os.path, "isfile", return_value=True), \
                patch.object(main, "find_game_process", return_value=None) as find_game, \
                patch.object(main, "_spawn") as spawn:
            result = await self.plugin.launch_trainer("1174180", "/trainers/new.exe")

        self.assertFalse(result["ok"])
        self.assertIn("still running", result["error"])
        find_game.assert_not_called()
        spawn.assert_not_called()
        self.assert_tracking_preserved()

    async def test_confirmed_sigkill_exit_counts_only_one_process(self):
        def exit_on_sigkill(pid, sig):
            if sig == signal.SIGKILL:
                self.alive.remove(str(pid))
                self.listening = False

        self.kill.side_effect = exit_on_sigkill

        result = await self.plugin.stop_trainer()

        self.assertTrue(result["ok"])
        self.assertEqual(result["killed"], 1)
        self.assertEqual(self.kill.call_count, 2)
        self.assert_tracking_cleared()

    async def test_sigkill_exit_is_counted_after_sigterm_was_denied(self):
        def deny_term_then_exit(pid, sig):
            if sig == signal.SIGTERM:
                raise PermissionError("not permitted")
            self.alive.remove(str(pid))
            self.listening = False

        self.kill.side_effect = deny_term_then_exit

        result = await self.plugin.stop_trainer()

        self.assertTrue(result["ok"])
        self.assertEqual(result["killed"], 1)
        self.assertEqual(self.kill.call_count, 2)
        self.assert_tracking_cleared()

    async def test_unauthenticated_listener_after_pid_exit_is_not_stopped(self):
        def exit_without_closing_listener(pid, sig):
            self.alive.discard(str(pid))

        self.kill.side_effect = exit_without_closing_listener

        result = await self.plugin.stop_trainer()

        self.assertFalse(result["ok"])
        self.assertFalse(result["apiUp"])
        self.assertTrue(result["error"])
        self.assert_tracking_preserved()

    async def test_no_visible_pids_with_unauthenticated_listener_is_not_stopped(self):
        self.alive.clear()

        result = await self.plugin.stop_trainer()

        self.assertFalse(result["ok"])
        self.assertEqual(result["killed"], 0)
        self.assertTrue(result["error"])
        self.kill.assert_not_called()
        self.assert_tracking_preserved()

    async def test_graceful_shutdown_waits_for_actual_exit(self):
        self.shutdown_reply = (200, {"deactivated": 10, "failed": 0})

        def after_shutdown_scan():
            if self.request.called:
                self.alive.clear()
                self.listening = False
            return set(self.alive)

        self.processes.side_effect = after_shutdown_scan

        result = await self.plugin.stop_trainer()

        self.assertTrue(result["ok"])
        self.assertEqual(result["killed"], 0)
        self.assertEqual(result["unhooked"], 10)
        self.kill.assert_not_called()
        self.assert_tracking_cleared()

    async def test_successful_shutdown_reply_does_not_hide_a_surviving_pid(self):
        self.shutdown_reply = (200, {"deactivated": 10, "failed": 0})
        self.kill.side_effect = PermissionError("not permitted")

        result = await self.plugin.stop_trainer()

        self.assertFalse(result["ok"])
        self.assertEqual(result["killed"], 0)
        self.assertEqual(result["unhooked"], 10)
        self.assertTrue(result["error"])
        self.assert_tracking_preserved()


if __name__ == "__main__":
    unittest.main()

"""A runner without a user bus must not attempt transient systemd scopes."""
import asyncio
import importlib.machinery
import importlib.util
import os
import io
import json
from pathlib import Path
import socket
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

loader = importlib.machinery.SourceFileLoader("fault_runtime", str(Path(__file__).with_name("run")))
spec = importlib.util.spec_from_loader(loader.name, loader)
runner = importlib.util.module_from_spec(spec)
loader.exec_module(runner)


class RuntimeTests(unittest.TestCase):
    def test_guardian_socket_root_fits_with_a_long_temp_directory(self):
        with tempfile.TemporaryDirectory(prefix="mfe-path-control-", dir="/tmp") as directory:
            long_temp = Path(directory) / ("long-" * 16)
            long_temp.mkdir()
            scratch = long_temp / "binary-scratch"
            with patch.dict(os.environ, {"TMPDIR": str(long_temp)}), \
                 patch.object(tempfile, "gettempdir", return_value=str(long_temp)):
                roots = runner.supervised_roots(scratch)
                name, prefix, parent = roots[0]
                self.assertEqual(name, "ST3_MESSAGING_ROOT")
                self.assertEqual(roots[1],
                                 ("ST3_MESSAGING_SCRATCH", "mfi-owner-", str(scratch)))
                with tempfile.TemporaryDirectory(prefix=prefix, dir=parent) as owned:
                    with tempfile.TemporaryDirectory(prefix="mfe-", dir=owned) as case:
                        pty_root = Path(case) / "cobalt" / "pty"
                        pty_root.mkdir(parents=True)
                        socket_path = pty_root / "eval.fault-probe.sock"
                        # Preserve the PTY implementation's portable 104-byte bound;
                        # exercising an actual Unix bind at the complete fixture path.
                        self.assertLessEqual(len(os.fsencode(socket_path)), 104)
                        with socket.socket(socket.AF_UNIX) as listener:
                            listener.bind(str(socket_path))

    def test_only_an_existing_user_bus_is_inherited(self):
        for mode in ("unset", "directory", "bus"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binary = root / "candidate"
                binary.write_text("fixture")
                runtime = root / "runtime"
                runtime.mkdir()
                environment = {} if mode == "unset" else {"XDG_RUNTIME_DIR": str(runtime)}
                with socket.socket(socket.AF_UNIX) as bus:
                    if mode == "bus":
                        bus.bind(str(runtime / "bus"))
                    with patch.dict(os.environ, environment, clear=True):
                        node = runner.Node(root, "amber", binary, {"PATH": "/bin"}, root / "scratch", 60)
                    if mode == "bus":
                        self.assertEqual(str(runtime), node.env["XDG_RUNTIME_DIR"])
                    else:
                        self.assertNotIn("XDG_RUNTIME_DIR", node.env)


class GiveBytesTests(unittest.TestCase):
    def test_a_link_shares_the_bytes_and_a_replace_does_not_touch_the_others(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.write_bytes(b"executable")
            first, second = root / "first", root / "second"
            runner.give_bytes(source, first)
            runner.give_bytes(source, second)
            self.assertEqual(first.stat().st_ino, source.stat().st_ino)
            # The swaps the cases perform: a new file renamed over a node's path.
            swap = root / "swap.next"
            swap.write_bytes(b"replacement")
            os.replace(swap, first)
            self.assertEqual(first.read_bytes(), b"replacement")
            self.assertEqual(second.read_bytes(), b"executable")
            self.assertEqual(source.read_bytes(), b"executable")

    def test_it_copies_only_where_a_link_is_impossible(self):
        import errno
        for number in (errno.EXDEV, errno.EPERM, errno.EACCES):
            with self.subTest(errno=errno.errorcode[number]), tempfile.TemporaryDirectory() as directory:
                source, copy = Path(directory, "source"), Path(directory, "copy")
                source.write_bytes(b"executable")
                with patch.object(runner.os, "link", side_effect=OSError(number, "no link")):
                    runner.give_bytes(source, copy)
                self.assertEqual(copy.read_bytes(), b"executable")
                self.assertNotEqual(copy.stat().st_ino, source.stat().st_ino)

    def test_any_other_error_is_not_hidden(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileNotFoundError):
                runner.give_bytes(Path(directory, "missing"), Path(directory, "target"))


class ChannelReadinessTests(unittest.IsolatedAsyncioTestCase):
    async def test_current_channel_waits_for_a_current_delivery_report(self):
        for card in ({}, {"delivery": None}, {"delivery": {"state": "legacy"}},
                     {"delivery": {"state": "current"}}):
            with self.subTest(card=card), patch.object(runner, "agent", AsyncMock(return_value=card)):
                result = await runner.current_channel(object())
            if (card.get("delivery") or {}).get("state") == "current":
                self.assertIs(result, card)
            else:
                self.assertIsNone(result)

    async def test_replacement_state_is_checked_after_the_card_await(self):
        old = {"pid": 10, "start_ticks": 100, "argv": " driver omp-channel "}
        child = {"pid": 20, "start_ticks": 200, "argv": " driver omp-channel "}
        baseline = {"subject": "agent/eval/fault-probe", "component": "delivery",
                    "transport": "omp-channel", "ready": True, "incarnation": "seat",
                    "pid": 10, "start_ticks": 100, "epoch": 1,
                    "token_sha256": "old", "sequence": 1}
        ready = {**baseline, "pid": 20, "start_ticks": 200, "epoch": 2,
                 "token_sha256": "new", "sequence": 2}
        for transition in ("unchanged", "not-ready", "dead", "replaced"):
            with self.subTest(transition=transition):
                state = {"processes": [child], "reports": [baseline, ready]}
                node = SimpleNamespace(seats=lambda: state["processes"])
                entered, release = asyncio.Event(), asyncio.Event()
                async def delayed_card(_):
                    entered.set()
                    await release.wait()
                    return {"incarnation_id": "seat", "delivery": {"state": "current"}}
                with patch.object(runner, "current_channel", delayed_card), \
                     patch.object(runner, "admitted_reports", lambda _: state["reports"]):
                    task = asyncio.create_task(runner.current_replacement_channel(node, baseline, old, "seat"))
                    await entered.wait()
                    if transition == "not-ready":
                        state["reports"].append({**ready, "sequence": 3, "ready": False})
                    elif transition == "dead":
                        state["processes"] = []
                    elif transition == "replaced":
                        state["processes"] = [{**child, "pid": 30, "start_ticks": 300}]
                    release.set()
                    result = await task
                if transition == "unchanged":
                    self.assertEqual(ready, result["report"])
                else:
                    self.assertIsNone(result)

    async def test_absent_agent_value_is_not_ready(self):
        node = AsyncMock()
        node.cli.return_value = '{"value":null}'
        self.assertEqual(await runner.agent(node), {})
        self.assertIsNone(await runner.current_channel(node))


class WaitDeadlineTests(unittest.IsolatedAsyncioTestCase):
    async def test_probe_timeout_does_not_abort_outer_readiness_wait(self):
        # The first CLI communicate() deadline used to escape until() immediately.
        ready = {"harness_state": "idle"}
        probe = AsyncMock(side_effect=[TimeoutError(), ready])
        signal = io.StringIO()
        with patch.object(runner.asyncio, "sleep", AsyncMock()), \
             patch.object(runner.sys, "stderr", signal):
            self.assertIs(await runner.until(probe, 30), ready)
        self.assertEqual(probe.await_count, 2)
        event = json.loads(signal.getvalue())
        self.assertEqual(event["ended_by"], "ready")
        self.assertEqual(event["probe_timeouts"], 1)
        self.assertEqual(event["outer_timeout_ms"], 30000)
        self.assertGreaterEqual(event["elapsed_ms"], 0)

    async def test_outer_deadline_cancels_an_in_flight_probe(self):
        # Expire the actual timeout after the probe enters, without wall-clock sleeps.
        scope = asyncio.timeout(3600)
        cancelled = asyncio.Event()
        async def blocked():
            scope.reschedule(asyncio.get_running_loop().time() - 1)
            try:
                await asyncio.Event().wait()
            finally:
                cancelled.set()
        signal = io.StringIO()
        with patch.object(runner.asyncio, "timeout", return_value=scope), \
             patch.object(runner.sys, "stderr", signal):
            with self.assertRaisesRegex(TimeoutError, "outer deadline"):
                await runner.until(blocked, 3600)
        self.assertTrue(cancelled.is_set())
        event = json.loads(signal.getvalue())
        self.assertEqual(event["ended_by"], "outer-deadline")
        self.assertEqual(event["probe_timeouts"], 0)
        self.assertGreaterEqual(event["elapsed_ms"], 0)

    async def test_non_timeout_probe_failure_is_not_retried(self):
        probe = AsyncMock(side_effect=RuntimeError("daemon exited"))
        with patch.object(runner.sys, "stderr", io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "daemon exited"):
                await runner.until(probe)
        probe.assert_awaited_once()


if __name__ == "__main__":
    unittest.main()

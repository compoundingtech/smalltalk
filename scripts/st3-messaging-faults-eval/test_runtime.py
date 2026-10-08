"""A runner without a user bus must not attempt transient systemd scopes."""
import importlib.machinery
import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import socket
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

loader = importlib.machinery.SourceFileLoader("fault_runtime", str(Path(__file__).with_name("run")))
spec = importlib.util.spec_from_loader(loader.name, loader)
runner = importlib.util.module_from_spec(spec)
loader.exec_module(runner)


class RuntimeTests(unittest.TestCase):
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
                        node = runner.Node(root, "amber", binary, {"PATH": "/bin"}, root / "scratch")
                    if mode == "bus":
                        self.assertEqual(str(runtime), node.env["XDG_RUNTIME_DIR"])
                    else:
                        self.assertNotIn("XDG_RUNTIME_DIR", node.env)


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

    async def test_absent_agent_value_is_not_ready(self):
        node = AsyncMock()
        node.cli.return_value = '{"value":null}'
        self.assertEqual(await runner.agent(node), {})
        self.assertIsNone(await runner.current_channel(node))


class FailureEvidenceTests(unittest.TestCase):
    def test_sparse_log_keeps_start_and_end_without_reading_the_middle(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            case = root / "harness-restart"
            case.mkdir()
            path = case / "cobalt-daemon.log"
            with path.open("wb") as stream:
                stream.write(b"fault injection context\n")
                stream.seek(32 * 1024 * 1024)
                stream.write(b"native read after recovery\n")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                runner.emit_failure_evidence(root, {"cases": [
                    {"case": "harness-restart", "verdict": "fail"}]})
            evidence = json.loads(output.getvalue())
            self.assertIn("fault injection context", evidence["head"])
            self.assertIn("native read after recovery", evidence["tail"])
            self.assertLessEqual(evidence["retained_bytes"], 32 * 1024)
            self.assertGreater(evidence["omitted_bytes"], 31 * 1024 * 1024)

    def test_failure_capture_has_one_budget_across_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for case in ("harness-restart", "receiver-down"):
                (root / case).mkdir()
                for node in ("amber", "cobalt"):
                    for suffix in ("replication.json", "driver-api-warnings.log", "daemon.log",
                                   "worker.log", "native.log", "error-agent.json", "error-trace.jsonl"):
                        (root / case / f"{node}-{suffix}").write_bytes(b"x" * 64 * 1024)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                runner.emit_failure_evidence(root, {"cases": [
                    {"case": case, "verdict": "fail"}
                    for case in ("harness-restart", "receiver-down")]})
            rows = [json.loads(line) for line in output.getvalue().splitlines()]
            self.assertLessEqual(sum(row.get("retained_bytes", 0) for row in rows), 256 * 1024)
            self.assertEqual("byte budget exhausted", rows[-1]["native_failure_evidence"])

    def test_passing_case_prints_no_evidence(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            runner.emit_failure_evidence(Path("/no-fixture-evidence"), {"cases": [
                {"case": "harness-restart", "verdict": "pass"}]})
        self.assertEqual("", output.getvalue())


if __name__ == "__main__":
    unittest.main()

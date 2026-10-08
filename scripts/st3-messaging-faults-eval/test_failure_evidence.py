"""Failure-log copies retain the diagnostic seam without reading arbitrary files."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest import mock

loader = importlib.machinery.SourceFileLoader(
    "failure_evidence", str(Path(__file__).resolve().parent.parent / "st3_failure_evidence.py"))
spec = importlib.util.spec_from_loader(loader.name, loader)
module = importlib.util.module_from_spec(spec)
loader.exec_module(module)


class FailureEvidenceTests(unittest.TestCase):
    def test_suspend_results_keep_refusals_with_bounded_first_and_last_attempts(self):
        capture = module.SuspendCliEvidence()
        command = ["st3", "agents", "suspend", "agent/eval/a"]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for i in range(3):
                capture.observe(command, subprocess.CompletedProcess(
                    command, 2, "", "refused-" + str(i) + "é" * 3000))
            self.assertEqual([], list(root.iterdir()))
            capture.write(root)
            record = json.loads((root / "suspend-cli-results.json").read_text())
            self.assertEqual(3, record["attempts"])
            self.assertEqual(1, record["omitted_attempts"])
            self.assertEqual([1, 3], [row["attempt"] for row in record["results"]])
            for row in record["results"]:
                self.assertEqual(2, row["exit"])
                self.assertEqual(command, row["command"])
                self.assertEqual(2048, row["stderr"]["bytes"] - row["stderr"]["omitted_bytes"])
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                module.emit_failure_files(root)
            self.assertEqual("suspend-cli-results.json", json.loads(
                output.getvalue())["native_failure_evidence"])

    def test_boot_cli_preserves_unchecked_stdout_and_checked_failure(self):
        loader = importlib.machinery.SourceFileLoader(
            "boot_failure_capture", str(Path(__file__).resolve().parent.parent / "st3-boot-canaries/run"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        boot = importlib.util.module_from_spec(spec)
        loader.exec_module(boot)
        node = boot.Node.__new__(boot.Node)
        node.binary, node.endpoint, node.env = Path("st3"), Path("st.sock"), {}
        node.suspend_cli_evidence = module.SuspendCliEvidence()
        result = subprocess.CompletedProcess([], 2, "invalid stdout", "seat busy")
        with mock.patch.object(boot.subprocess, "run", return_value=result) as call:
            self.assertEqual("invalid stdout", node.cli("agents", "show", "a", check=False))
            self.assertEqual(0, node.suspend_cli_evidence.attempts)
            self.assertEqual("invalid stdout", node.cli("agents", "suspend", "a", check=False))
            with self.assertRaisesRegex(boot.Failure, "seat busy invalid stdout"):
                node.cli("agents", "suspend", "a")
            self.assertEqual(3, call.call_count)
            self.assertEqual(2, node.suspend_cli_evidence.attempts)
            self.assertEqual("seat busy", node.suspend_cli_evidence.first["stderr"]["tail"])

    def test_hook_receipts_and_sparse_log_tail_survive_with_explicit_omission(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "receipts-a.jsonl").write_text('{"event":"hook","exit":126}\n')
            with (root / "daemon.log").open("wb") as stream:
                stream.write(b"first failure\n")
                stream.seek(32 * 1024 * 1024)
                stream.write(b"final diagnostic\n")
            (root / "unrelated-secret.txt").write_text("must not appear")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                module.emit_failure_files(root)
            records = [json.loads(line) for line in output.getvalue().splitlines()]
            self.assertEqual(["receipts-a.jsonl", "daemon.log"],
                             [row["native_failure_evidence"] for row in records])
            self.assertIn("first failure", records[1]["head"])
            self.assertIn("final diagnostic", records[1]["tail"])
            self.assertEqual(16384, records[1]["retained_bytes"])
            self.assertEqual(records[1]["file_bytes"] - 16384, records[1]["omitted_bytes"])
            self.assertIn('"exit":126', records[0]["head"])
            self.assertNotIn("must not appear", output.getvalue())

    def test_fifo_and_symlink_are_rejected_without_reading_their_contents(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            os.mkfifo(root / "daemon.log")
            (root / "secret.txt").write_text("must not appear")
            (root / "driver-0.log").symlink_to(root / "secret.txt")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                module.emit_failure_files(root)
            rows = [json.loads(line) for line in output.getvalue().splitlines()]
            self.assertEqual(2, len(rows))
            self.assertTrue(all("capture_error" in row for row in rows))
            self.assertNotIn("must not appear", output.getvalue())

    def test_messaging_capture_uses_shared_reader_and_only_failed_cases(self):
        loader = importlib.machinery.SourceFileLoader("messaging_failure_capture", str(
            Path(__file__).resolve().parent / "run"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        messaging = importlib.util.module_from_spec(spec)
        loader.exec_module(messaging)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("passed", "failed"):
                (root / name).mkdir()
                (root / name / "trace.json").write_text(name)
            os.mkfifo(root / "failed" / "amber-daemon.log")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                messaging.emit_failure_evidence(root, {"cases": [
                    {"case": "passed", "verdict": "pass"},
                    {"case": "failed", "verdict": "fail"}]})
            rows = [json.loads(line) for line in output.getvalue().splitlines()]
            self.assertEqual(["failed/trace.json", "failed/amber-daemon.log"],
                             [row["native_failure_evidence"] for row in rows])
            self.assertEqual("failed", rows[0]["head"])
            self.assertIn("capture_error", rows[1])

    def test_escaped_payloads_share_a_raw_byte_budget_across_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for i in range(20):
                (root / f"driver-{i:02d}.log").write_bytes(b"\0" * 32768)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                module.emit_failure_files(root)
            records = [json.loads(line) for line in output.getvalue().splitlines()]
            self.assertEqual(256 * 1024, sum(row.get("retained_bytes", 0) for row in records))
            self.assertEqual("byte budget exhausted", records[-1]["native_failure_evidence"])
            self.assertGreater(len(output.getvalue()), 256 * 1024)


if __name__ == "__main__":
    unittest.main()

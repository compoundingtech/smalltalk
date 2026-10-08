"""Supervisor controls only; fake children never supply source/Ready evidence."""
import contextlib
import hashlib
import importlib.machinery
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

MODULE = importlib.machinery.SourceFileLoader(
    "qualify_agent_source", str(Path(__file__).resolve().parents[1] / "qualify-agent-source")
).load_module()


class SupervisorTests(unittest.TestCase):
    def child(self, root, code):
        binary = root / "fixture-child"
        binary.write_text("#!/usr/bin/python3\n" + code)
        binary.chmod(0o700)
        return binary

    def test_success_retains_bounded_output_and_actual_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child = self.child(root, "print('supervisor fixture')\n")
            code, reason, output, _ = MODULE.supervise(child, root / "unused", "fixture", root, 2)
            self.assertEqual(code, 0)
            self.assertIsNone(reason)
            self.assertEqual(output["stdout"], b"supervisor fixture\n")

    def test_wall_deadline_terminates_child(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child = self.child(root, "import time\ntime.sleep(10)\n")
            code, reason, _, elapsed = MODULE.supervise(child, root / "unused", "fixture", root, 0.1)
            self.assertNotEqual(code, 0)
            self.assertEqual(reason, "TIMEOUT")
            self.assertLess(elapsed, 3)

    def test_output_budget_stops_without_retaining_unbounded_data(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child = self.child(root, "import sys\nwhile True: sys.stdout.buffer.write(b'x'*65536)\n")
            code, reason, output, _ = MODULE.supervise(child, root / "unused", "fixture", root, 2)
            self.assertNotEqual(code, 0)
            self.assertEqual(reason, "OUTPUT_LIMIT")
            self.assertLessEqual(len(output["stdout"]), MODULE.OUTPUT_LIMIT)

    def test_preexec_failure_retains_phase_and_never_executes_child(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ran = root / "child-ran"
            child = self.child(root, f"from pathlib import Path\nPath({str(ran)!r}).touch()\n")
            def refuse(limit, _bounds):
                if limit == MODULE.resource.RLIMIT_AS:
                    raise OSError(22, "fixture unsupported bound")
            with mock.patch.object(MODULE.resource, "setrlimit", side_effect=refuse):
                code, reason, output, _ = MODULE.supervise(child, root / "unused", "fixture", root, 2)
            self.assertIsNone(code)
            self.assertIn("PREEXEC_FAILED: RLIMIT_AS: OSError", reason)
            self.assertIn(b"fixture unsupported bound", output["stderr"])
            self.assertFalse(ran.exists())

    def test_preexec_failure_keeps_single_use_attempt_and_private_diagnostic(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ran = root / "child-ran"
            child = self.child(root, f"from pathlib import Path\nPath({str(ran)!r}).touch()\n")
            (root / ".ivm-qualification-scratch").write_text("fixture")
            (root / "backup.sqlite").write_bytes(b"not opened by the child")
            receipt = root / "backup-receipt.json"
            receipt.write_text(json.dumps({"method": "sqlite-online-backup", "completed": True,
                "original_receiver_sha256": hashlib.sha256(b"fixture").hexdigest()}))
            arguments = ["qualify-agent-source", "--binary", str(child),
                "--binary-sha256", MODULE.digest(child), "--source-head", "a" * 40,
                "--source-tree", "b" * 40, "--scratch", str(root),
                "--original-receiver", "fixture", "--backup-receipt", str(receipt)]
            def refuse(limit, _bounds):
                if limit == MODULE.resource.RLIMIT_AS:
                    raise OSError(22, "fixture unsupported bound")
            with mock.patch.object(MODULE.sys, "argv", arguments), \
                 mock.patch.object(MODULE.resource, "setrlimit", side_effect=refuse), \
                 contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(MODULE.main(), 2)
            result = json.loads((root / "qualification.json").read_text())
            self.assertEqual(result["result"], "UNAVAILABLE")
            self.assertIsNone(result["exit_code"])
            self.assertTrue((root / ".qualification-started").is_file())
            self.assertIn("RLIMIT_AS", result["stop_reason"])
            self.assertIn("fixture unsupported bound", (root / "qualification.stderr").read_text())
            self.assertFalse(ran.exists())
            with mock.patch.object(MODULE.sys, "argv", arguments), \
                 contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                MODULE.main()

    def test_scratch_symlinks_refuse_before_source_open(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "foreign").symlink_to("/tmp")
            with self.assertRaisesRegex(RuntimeError, "symlinks"):
                MODULE.footprint(root)


if __name__ == "__main__":
    unittest.main()

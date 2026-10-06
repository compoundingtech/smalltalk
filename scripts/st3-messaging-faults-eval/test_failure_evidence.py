"""Failure-log copies retain the diagnostic seam without reading arbitrary files."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest

loader = importlib.machinery.SourceFileLoader(
    "failure_evidence", str(Path(__file__).resolve().parent.parent / "st3_failure_evidence.py"))
spec = importlib.util.spec_from_loader(loader.name, loader)
module = importlib.util.module_from_spec(spec)
loader.exec_module(module)


class FailureEvidenceTests(unittest.TestCase):
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

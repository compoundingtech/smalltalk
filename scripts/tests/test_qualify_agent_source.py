"""Supervisor controls only; fake children never supply source/Ready evidence."""
import importlib.machinery
from pathlib import Path
import tempfile
import unittest

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

    def test_scratch_symlinks_refuse_before_source_open(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "foreign").symlink_to("/tmp")
            with self.assertRaisesRegex(RuntimeError, "symlinks"):
                MODULE.footprint(root)


if __name__ == "__main__":
    unittest.main()

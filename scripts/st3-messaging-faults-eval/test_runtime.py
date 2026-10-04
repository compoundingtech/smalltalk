"""A runner without a user bus must not attempt transient systemd scopes."""
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import socket
import tempfile
import unittest
from unittest.mock import patch

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


if __name__ == "__main__":
    unittest.main()

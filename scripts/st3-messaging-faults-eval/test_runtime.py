"""A runner without a user bus must not attempt transient systemd scopes."""
import asyncio
import importlib.machinery
import importlib.util
import os
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


if __name__ == "__main__":
    unittest.main()

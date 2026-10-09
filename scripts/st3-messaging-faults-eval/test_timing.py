"""Recovery diagnostics retain their own origin and independent native evidence."""
import importlib.machinery
import importlib.util
from pathlib import Path
import unittest

loader = importlib.machinery.SourceFileLoader("fault_timing", str(Path(__file__).with_name("run")))
spec = importlib.util.spec_from_loader(loader.name, loader)
runner = importlib.util.module_from_spec(spec)
loader.exec_module(runner)


class TimingTests(unittest.TestCase):
    def test_replacement_barrier_requires_a_later_admitted_ready_owner(self):
        old = {"pid": 10, "start_ticks": 100}
        child = {"pid": 20, "start_ticks": 200}
        baseline = {"subject": "agent/eval/fault-probe", "component": "delivery",
                    "transport": "omp-channel", "ready": True, "incarnation": "seat",
                    "pid": 10, "start_ticks": 100, "epoch": 1,
                    "token_sha256": "old-binding", "sequence": 1}
        good = {**baseline, **child, "epoch": 2, "token_sha256": "new-binding", "sequence": 2}
        def check(row, process=child):
            return runner.replacement_ready_report([baseline, row], baseline, old, process, "seat")
        self.assertIsNone(check(baseline))  # Retained current beat from the killed child.
        self.assertIsNone(runner.replacement_ready_report([baseline], baseline, old, child, "seat"))
        for fields in ({"ready": False}, {"epoch": 1}, {"sequence": 1},
                       {"token_sha256": "old-binding"}, {"incarnation": "previous"},
                       {"component": "title"}, {"transport": "foreign"}, {"start_ticks": 199}):
            with self.subTest(fields=fields):
                self.assertIsNone(check({**good, **fields}))
        self.assertIsNone(check(good, old))
        self.assertEqual(good, check(good))
        # PID reuse is distinct only when the daemon observed the same new birth identity.
        reused = {"pid": 10, "start_ticks": 201}
        self.assertEqual({**good, **reused}, check({**good, **reused}, reused))
        self.assertIsNone(check({**good, **reused}, old))

    def test_recovery_does_not_use_fresh_mail_or_transport_probes_as_its_origin(self):
        timing = runner.recovery_timing("message/recovered", 1000, 900, "new-incarnation", [
            {"event": "request", "at_unix_ms": 999, "line": "POST /v1/peer/exchange HTTP/1.1"},
            {"event": "request", "at_unix_ms": 1001, "line": "HEAD /v1/peer/exchange HTTP/1.1"},
            {"event": "request", "at_unix_ms": 1100, "line": "POST /v1/peer/exchange HTTP/1.1"},
            {"event": "request", "at_unix_ms": 26000, "line": "POST /v1/peer/exchange HTTP/1.1"},
        ], [
            {"event": "received", "subject": "message/fresh", "at_unix_ms": 26100},
            {"event": "received", "subject": "message/recovered", "at_unix_ms": 28000},
            {"event": "read", "subject": "message/recovered", "at_unix_ms": 28110,
             "accepted_at_unix_ms": 28004},
        ], [
            {"kind": "message.staged", "subject": "message/fresh", "accepted_at_unix_ms": 26099},
            {"kind": "message.staged", "subject": "message/recovered", "accepted_at_unix_ms": 27999},
        ])
        self.assertEqual("new-incarnation", timing["channel_incarnation"])
        self.assertEqual({"channel_ready_observed": -100, "partition_cleared": 0,
                          "first_peer_exchange_request": 100, "staged_accepted": 26999,
                          "native_received": 27000, "read_accepted": 27004},
                         timing["after_partition_clear_ms"])

    def test_a_graph_read_without_native_consumption_does_not_fabricate_native_phases(self):
        timing = runner.recovery_timing("message/recovered", 1000, None, None, [], [], [
            {"kind": "message.read", "subject": "message/recovered", "accepted_at_unix_ms": 1100},
        ])
        self.assertIsNone(timing["at_unix_ms"]["native_received"])
        self.assertIsNone(timing["at_unix_ms"]["read_accepted"])
        self.assertIsNone(timing["at_unix_ms"]["staged_accepted"])


if __name__ == "__main__":
    unittest.main()

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

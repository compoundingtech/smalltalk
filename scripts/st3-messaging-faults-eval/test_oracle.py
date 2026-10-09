"""Negative controls: a graph receipt alone must never pass native delivery."""
import copy
import importlib.machinery
import importlib.util
from pathlib import Path
import unittest

loader = importlib.machinery.SourceFileLoader("fault_eval", str(Path(__file__).with_name("run")))
spec = importlib.util.spec_from_loader(loader.name, loader)
runner = importlib.util.module_from_spec(spec)
loader.exec_module(runner)


class OracleTests(unittest.TestCase):
    def setUp(self):
        self.proof = {
            "case": "daemon-restart", "read_after_clear_ms": 900,
            "received_copies": 1, "warmup_received_copies": 1,
            "graph_read_claims": 1, "read_receipt_matches_graph": True,
            "provider_starts": 1, "expected_provider_starts": 1,
            "provider_pids_before": [123], "provider_pids_after": [123],
            "incarnation_before": "123:one", "incarnation_after": "123:one",
            "delivery_after": {"state": "current"}, "sender_message_status": "closed",
            "seat_images_after": ["/next/st3"], "daemon_image": "/next/st3",
            "recovered_received_copies": 1, "recovered_read_claims": 1,
            "recovered_sender_status": "closed", "recovered_recipient_status": "closed",
            "recovered_receipt_matches_graph": True, "recovered_read_after_clear_ms": 900,
        }

    def test_independent_evidence_agrees(self):
        self.assertEqual([], runner.judge(self.proof))

    def test_false_positive_controls(self):
        for change, reason in [
            ({"received_copies": 0}, "received 0 copies"),
            ({"projected_message_files": 1}, "file-mailbox messages"),
            ({"driver_catalog_files": 1}, "fabricated catalogs"),
            ({"received_copies": 2}, "received 2 copies"),
            ({"warmup_received_copies": 2}, "already-read warmup mail reached the provider 2 times"),
            ({"read_after_clear_ms": None}, "not read"),
            ({"read_after_clear_ms": 10001}, "limit 10000"),
            ({"delivered_during_outage": True}, "before the injected outage cleared"),
            ({"graph_read_claims": 0}, "0 read claims"),
            ({"graph_read_claims": 2}, "2 read claims"),
            ({"read_receipt_matches_graph": False}, "read time differ"),
            ({"provider_starts": 2}, "started 2 times"),
            ({"provider_pids_after": [456]}, "provider process changed"),
            ({"incarnation_after": "123:two"}, "incarnation changed"),
            ({"incarnation_before": None, "incarnation_after": None}, "incarnation changed"),
            ({"delivery_after": {"state": "stale"}}, "report is stale"),
            ({"seat_images_after": ["/next/st3", "/old/st3"]}, "still run a replaced binary: /old/st3"),
            ({"sender_message_status": "sent"}, "not converged"),
            ({"recovered_received_copies": 0}, "exactly one native offer and read"),
            ({"recovered_received_copies": 2}, "exactly one native offer and read"),
            ({"recovered_read_claims": 0}, "exactly one native offer and read"),
            ({"recovered_read_claims": 2}, "exactly one native offer and read"),
            ({"recovered_receipt_matches_graph": False}, "recovered reader receipt"),
            ({"recovered_sender_status": "sent"}, "both mailboxes"),
            ({"recovered_recipient_status": "sent"}, "both mailboxes"),
            ({"recovered_read_after_clear_ms": -1}, "recovery missed"),
            ({"recovered_read_after_clear_ms": 10001}, "recovery missed"),
            ({"recovered_read_after_clear_ms": None}, "recovery missed"),
        ]:
            with self.subTest(change=change):
                proof = copy.deepcopy(self.proof)
                proof.update(change)
                self.assertIn(reason, "; ".join(runner.judge(proof)))

    def test_previous_release_channel_must_reexec_without_replacement(self):
        self.proof.update(case="old-channel", channel_pids_before=[789],
                          channel_pids_after=[789], previous_release_channel_observed=True)
        self.assertEqual([], runner.judge(self.proof))
        for change, reason in [
            ({"channel_pids_after": [790]}, "same process"),
            ({"channel_pids_before": []}, "same process"),
            ({"previous_release_channel_observed": False}, "not observed"),
            ({"delivery_after": {"state": "legacy"}}, "report is legacy"),
            ({"seat_images_after": ["/release/st3", "/next/st3"]}, "still run a replaced binary"),
        ]:
            proof = copy.deepcopy(self.proof)
            proof.update(change)
            self.assertIn(reason, "; ".join(runner.judge(proof)))

    def test_a_failed_handoff_requires_visibility_in_all_three_views(self):
        self.proof.update(case="handoff-failed", failed_path_visible=True,
                          failed_doctor_visible=True, failed_sender_visible=True)
        self.assertEqual([], runner.judge(self.proof))
        for field in ("failed_path_visible", "failed_doctor_visible", "failed_sender_visible"):
            proof = copy.deepcopy(self.proof)
            proof[field] = False
            self.assertIn(field, "; ".join(runner.judge(proof)))

    def test_injected_harness_restart_allows_exactly_one_replacement(self):
        self.proof.update(case="harness-restart", provider_starts=2, expected_provider_starts=2,
                          provider_pids_after=[456], incarnation_after="456:two")
        self.assertEqual([], runner.judge(self.proof))
        self.proof["provider_starts"] = 3
        self.assertIn("started 3 times", "; ".join(runner.judge(self.proof)))


if __name__ == "__main__":
    unittest.main()

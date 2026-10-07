#!/usr/bin/env python3
"""Tests for scripts/sql_advisory.py. Run: python3 scripts/sql_advisory_test.py"""
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import sql_advisory as advisory


def cost(statements, items, repeat=1, shape="SELECT body FROM claims WHERE subject=?"):
    return {
        "statements": statements,
        "items": items,
        "max_repeat": repeat,
        "top_shape": shape,
        "error": None,
    }


REPORT = {
    "small": {
        # A seeded N+1: one lookup per item, so statements grow with the items returned.
        "GET /n-plus-one": cost(3 + 10 * 4, 10, repeat=10 * 4),
        # A flat route: the same few statements however many items it returns.
        "GET /flat": cost(12, 10, repeat=3),
        # A list that grows its answer but reads it in two statements.
        "GET /batched": cost(6, 10, repeat=2),
        # Many statements but not per item: a fixed fan-out, no growth.
        "GET /fixed-fanout": cost(40, 1, repeat=1),
    },
    "large": {
        "GET /n-plus-one": cost(3 + 100 * 4, 100, repeat=100 * 4),
        "GET /flat": cost(12, 100, repeat=3),
        "GET /batched": cost(6, 100, repeat=2),
        "GET /fixed-fanout": cost(40, 1, repeat=1),
    },
}


class Detector(unittest.TestCase):
    def test_flags_a_seeded_n_plus_one_and_stays_quiet_on_flat_routes(self):
        found = advisory.findings(REPORT)
        routes = {(f["route"], f["detector"]) for f in found}
        self.assertIn(("GET /n-plus-one", "n-plus-one-growth"), routes)
        self.assertIn(("GET /n-plus-one", "n-plus-one-repeat"), routes)
        for quiet in ("GET /flat", "GET /batched", "GET /fixed-fanout"):
            self.assertFalse([r for r in routes if r[0] == quiet], quiet)

    def test_a_repeat_below_the_floor_is_not_flagged(self):
        report = json.loads(json.dumps(REPORT))
        report["large"]["GET /flat"] = cost(30, 5, repeat=19)
        report["small"]["GET /flat"] = cost(30, 5, repeat=19)
        self.assertFalse([f for f in advisory.findings(report) if f["route"] == "GET /flat"])

    def test_one_text_run_once_per_item_is_flagged_even_when_statements_do_not_grow(self):
        report = {"small": {"GET /x": cost(60, 50, repeat=50)}, "large": {"GET /x": cost(60, 50, repeat=50)}}
        found = advisory.findings(report)
        self.assertEqual([f["detector"] for f in found], ["n-plus-one-repeat"])

    def test_errors_are_not_judged(self):
        report = json.loads(json.dumps(REPORT))
        report["large"]["GET /n-plus-one"]["error"] = "503"
        self.assertFalse([f for f in advisory.findings(report) if f["route"] == "GET /n-plus-one"])


class Budget(unittest.TestCase):
    def test_new_known_worse_and_stale(self):
        found = advisory.findings(REPORT)
        growth = next(f for f in found if f["detector"] == "n-plus-one-growth")
        budget = {
            "entries": [
                {"route": "GET /n-plus-one", "detector": "n-plus-one-growth", "ceiling": growth["value"], "reason": "r", "owner": "o"},
                {"route": "GET /n-plus-one", "detector": "n-plus-one-repeat", "ceiling": 10, "reason": "r", "owner": "o"},
                {"route": "GET /gone", "detector": "n-plus-one-growth", "ceiling": 5, "reason": "fixed", "owner": "o"},
            ]
        }
        classified, stale = advisory.classify(found, budget)
        status = {f["detector"]: f["status"] for f in classified}
        self.assertEqual(status["n-plus-one-growth"], "known")
        self.assertEqual(status["n-plus-one-repeat"], "worse")
        self.assertEqual([e["route"] for e in stale], ["GET /gone"])
        _, stale = advisory.classify(found, {})
        self.assertEqual(stale, [])
        self.assertTrue(all(f["status"] == "new" for f in advisory.classify(found, {})[0]))

    def test_the_report_leads_with_new_findings(self):
        found, stale = advisory.classify(advisory.findings(REPORT), {})
        text = advisory.render(found, stale)
        self.assertIn("never blocks a merge", text)
        self.assertIn("2 new or worse", text)


class Main(unittest.TestCase):
    def test_exits_zero_whatever_it_finds_and_when_the_report_is_broken(self):
        with tempfile.TemporaryDirectory() as directory:
            good = os.path.join(directory, "report.json")
            json.dump(REPORT, open(good, "w"))
            self.assertEqual(advisory.main([good, "--budget", os.path.join(directory, "none.json"), "--summary", ""]), 0)
            broken = os.path.join(directory, "broken.json")
            open(broken, "w").write("not json")
            self.assertEqual(advisory.main([broken, "--summary", ""]), 0)


if __name__ == "__main__":
    unittest.main()

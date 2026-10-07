#!/usr/bin/env python3
"""Advisory SQL report: read the cost check's JSON report and say which routes look like N+1.

Advisory only. It never fails a build: it prints a markdown report, appends it to the GitHub
job summary when one is given, and exits 0 whatever it finds. See
doc/fleet/smalltalk/sql-audit/2026-10-07/advisory-sql-ci-design for the three detectors; this
file holds the N+1 detector and the report and budget machinery the others plug into.

The cost check (crates/st3/tests/daemon_cost.rs) writes the report when ST_COST_REPORT names a
file: per route, at a smaller and a ten-times-larger generated store, its statements, VM steps,
the number of items the answer returned and the most times one statement text ran.

N+1 detectors, per route, from the larger store's numbers:
  n-plus-one-growth   statements grew by more than 1.5x (plus a slack of 10) from the smaller to
                      the larger store while the answer returned more items. Statements should
                      not grow with the number of items.
  n-plus-one-repeat   one statement text ran at least 20 times and either at least once per
                      item of the answer's largest unit, or between one and five times per item
                      of any smaller unit of five or more (a lookup per run in an answer that
                      also lists a thousand steps). Units are the lengths of the answer's lists
                      of objects and the steps nested in them.

Overfetch detectors, per route, from the larger store's numbers (work per byte of answer; VM
steps stand in for rows read, since SQLite's statement counters have no rows-read figure):
  overfetch-steps     more than 50,000 VM steps, and more than 40 times the median VM steps per
                      answer byte of all judged routes. The answer size is floored at 256 bytes,
                      so a tiny answer is judged on its steps.
  overfetch-scan      more than 20,000 full-scan steps, and more than 40 full-scan steps per
                      answer byte (floored at 256). A table scan should not be the way a small
                      answer is found.

Only a route that was measured at both scales without an error is judged. A budget entry for a
route that failed or was not measured is kept and reported as not judged, never as fixed.
"""
import argparse
import json
import os
import sys

GROWTH = 1.5
SLACK = 10
REPEAT_FLOOR = 20
ANSWER_FLOOR_BYTES = 256
STEPS_MIN = 50_000
STEPS_MULTIPLE = 40
SCAN_MIN = 20_000
SCAN_PER_BYTE = 40


def judged_routes(report):
    """The routes measured at both scales without an error, and the ones that failed."""
    small, large = report.get("small", {}), report.get("large", {})
    judged, failed = set(), {}
    for route, after in large.items():
        before = small.get(route)
        error = after.get("error") or (before or {}).get("error")
        if before is None:
            continue
        if error:
            failed[route] = str(error)
        else:
            judged.add(route)
    return judged, failed


def repeats_per_item(repeat, units):
    """Whether a statement text ran like a loop over the answer: as often as the largest unit, or
    once to five times per item of a smaller unit."""
    if repeat < REPEAT_FLOOR:
        return False
    units = units or [1]
    return repeat >= max(units) or any(u >= 5 and u <= repeat <= 5 * u for u in units)


def per_byte(cost, field):
    return cost.get(field, 0) / max(cost.get("answer", 0), ANSWER_FLOOR_BYTES)


def median(values):
    values = sorted(values)
    if not values:
        return 0
    middle = len(values) // 2
    return values[middle] if len(values) % 2 else (values[middle - 1] + values[middle]) / 2


def overfetch_findings(report, judged):
    found = []
    large = report.get("large", {})
    ratios = [per_byte(large[route], "vm_steps") for route in judged if route in large]
    steps_limit = STEPS_MULTIPLE * median(ratios)
    for route in sorted(judged):
        cost = large.get(route)
        if cost is None:
            continue
        steps_ratio = per_byte(cost, "vm_steps")
        if cost.get("vm_steps", 0) > STEPS_MIN and steps_ratio > steps_limit:
            found.append(
                {
                    "route": route,
                    "detector": "overfetch-steps",
                    "value": round(steps_ratio, 1),
                    "detail": f"{cost['vm_steps']} VM steps for {cost.get('answer', 0)} answer bytes; the limit is {round(steps_limit, 1)} steps per byte (median x {STEPS_MULTIPLE})",
                }
            )
        scan_ratio = per_byte(cost, "fullscan_steps")
        if cost.get("fullscan_steps", 0) > SCAN_MIN and scan_ratio > SCAN_PER_BYTE:
            found.append(
                {
                    "route": route,
                    "detector": "overfetch-scan",
                    "value": round(scan_ratio, 1),
                    "detail": f"{cost['fullscan_steps']} full-scan steps for {cost.get('answer', 0)} answer bytes",
                }
            )
    return found


def findings(report):
    """Every detector hit in a cost report, as dicts with the value to compare to a ceiling."""
    found = []
    small, large = report.get("small", {}), report.get("large", {})
    judged, _ = judged_routes(report)
    found += overfetch_findings(report, judged)
    for route, after in sorted(large.items()):
        before = small.get(route)
        if route not in judged:
            continue
        items_before, items_after = before.get("items", 1), after.get("items", 1)
        statements_before, statements_after = before["statements"], after["statements"]
        if (
            items_after > items_before
            and statements_after > GROWTH * statements_before + SLACK
        ):
            found.append(
                {
                    "route": route,
                    "detector": "n-plus-one-growth",
                    "value": round(statements_after / max(statements_before, 1), 2),
                    "detail": f"statements {statements_before} -> {statements_after} while items {items_before} -> {items_after}",
                }
            )
        repeat, items = after.get("max_repeat", 0), max(after.get("items", 1), 1)
        units = after.get("item_units") or [items]
        if repeats_per_item(repeat, units):
            shape = (after.get("top_shape") or "").replace("\n", " ")
            found.append(
                {
                    "route": route,
                    "detector": "n-plus-one-repeat",
                    "value": repeat,
                    "detail": f"one statement ran {repeat} times for {'/'.join(str(u) for u in units)} items: {shape[:110]}",
                }
            )
    return found


def classify(found, budget, judged=None):
    """Mark each finding new, known (within its budgeted ceiling) or worse (over it). Returns the
    findings, the budget entries that no longer fire on a route that was judged, and the entries
    whose route was not judged this run. With no `judged` set every route counts as judged."""
    entries = {(e["route"], e["detector"]): e for e in budget.get("entries", [])}
    seen = set()
    for finding in found:
        key = (finding["route"], finding["detector"])
        seen.add(key)
        entry = entries.get(key)
        if entry is None:
            finding["status"] = "new"
        elif finding["value"] > entry["ceiling"]:
            finding["status"] = "worse"
            finding["ceiling"] = entry["ceiling"]
            finding["owner"] = entry.get("owner", "")
        else:
            finding["status"] = "known"
            finding["ceiling"] = entry["ceiling"]
            finding["owner"] = entry.get("owner", "")
    stale, unjudged = [], []
    for key, entry in entries.items():
        if key in seen:
            continue
        if judged is None or entry["route"] in judged:
            stale.append(entry)
        else:
            unjudged.append(entry)
    return found, stale, unjudged


ORDER = {"new": 0, "worse": 1, "known": 2}


def render(found, stale, unjudged=(), failed=None):
    lines = ["## Advisory SQL report", "", "Advisory only: this report never blocks a merge.", ""]
    fresh = [f for f in found if f["status"] in ("new", "worse")]
    known = [f for f in found if f["status"] == "known"]
    lines.append(
        f"{len(fresh)} new or worse, {len(known)} known, {len(stale)} budget entries no longer fire."
    )
    lines.append("")
    if found:
        lines += ["| status | route | detector | value | ceiling | detail |", "| --- | --- | --- | --- | --- | --- |"]
        for f in sorted(found, key=lambda f: (ORDER[f["status"]], f["route"], f["detector"])):
            lines.append(
                f"| {f['status']} | `{f['route']}` | {f['detector']} | {f['value']} | {f.get('ceiling', '')} | {f['detail']} |"
            )
        lines.append("")
    if stale:
        lines.append("Budget entries that no longer fire (remove them):")
        for entry in stale:
            lines.append(f"- `{entry['route']}` {entry['detector']}: {entry.get('reason', '')}")
        lines.append("")
    if failed:
        lines.append("Routes that failed or were not measured, so not judged (their budget entries stay):")
        for route, error in sorted(failed.items()):
            lines.append(f"- `{route}`: {error[:120]}")
        lines.append("")
    if unjudged:
        lines.append("Budget entries whose route was not judged this run, kept, not fixed:")
        for entry in unjudged:
            lines.append(f"- `{entry['route']}` {entry['detector']}")
        lines.append("")
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("report", help="the JSON file ST_COST_REPORT wrote")
    parser.add_argument("--budget", default=".github/sql-advisory-budget.json")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY"))
    args = parser.parse_args(argv)
    try:
        report = json.load(open(args.report))
        budget = json.load(open(args.budget)) if os.path.exists(args.budget) else {}
        judged, failed = judged_routes(report)
        found, stale, unjudged = classify(findings(report), budget, judged)
        text = render(found, stale, unjudged, failed)
    except Exception as error:  # Advisory: a broken report must not break the build.
        text = f"## Advisory SQL report\n\nThe report could not be built: {error}\n"
    print(text)
    if args.summary:
        with open(args.summary, "a") as summary:
            summary.write(text + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())

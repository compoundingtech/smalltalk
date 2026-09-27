#!/usr/bin/env python3
"""Summarize measurement windows as Markdown tables.

Usage: summarize.py OUT
"""

import csv
import json
import os
import statistics
import sys

TICKS = os.sysconf("SC_CLK_TCK")


def window(path):
    with open(os.path.join(path, "samples.csv")) as handle:
        rows = [{key: int(value) for key, value in row.items()} for row in csv.DictReader(handle)]
    with open(os.path.join(path, "window.json")) as handle:
        warmup = json.load(handle)["warmup_minutes"]
    measured = rows[warmup:]
    minutes = [
        {
            "cpu_s": (after["cpu_ticks"] - before["cpu_ticks"]) / TICKS,
            "switches": (after["voluntary_switches"] + after["involuntary_switches"])
            - (before["voluntary_switches"] + before["involuntary_switches"]),
        }
        for before, after in zip(measured, measured[1:])
    ]
    cpu = [minute["cpu_s"] for minute in minutes]
    switches = [minute["switches"] for minute in minutes]
    with open(os.path.join(path, "probe.json")) as handle:
        probes = json.load(handle)
    return {
        "minutes": len(minutes),
        "cpu_mean": statistics.mean(cpu),
        "cpu_sd": statistics.stdev(cpu) if len(cpu) > 1 else 0.0,
        "cpu_max": max(cpu),
        "switches_mean": statistics.mean(switches),
        "rss_start": measured[0]["rss_kib"] / 1024,
        "rss_end": measured[-1]["rss_kib"] / 1024,
        "rss_max": max(row["rss_kib"] for row in measured) / 1024,
        "hwm": measured[-1]["hwm_kib"] / 1024,
        "probes": probes,
    }


def main():
    out = sys.argv[1]
    names = sorted(name for name in os.listdir(out) if os.path.exists(os.path.join(out, name, "probe.json")))
    windows = {name: window(os.path.join(out, name)) for name in names}
    print("| Window | Minutes | CPU s/min mean (sd, max) | Switches/min | RSS MiB start / end / max | Peak RSS MiB |")
    print("| --- | ---: | ---: | ---: | ---: | ---: |")
    for name, data in windows.items():
        print(
            f"| `{name}` | {data['minutes']} | {data['cpu_mean']:.3f} ({data['cpu_sd']:.3f}, {data['cpu_max']:.2f}) "
            f"| {data['switches_mean']:.0f} | {data['rss_start']:.1f} / {data['rss_end']:.1f} / {data['rss_max']:.1f} "
            f"| {data['hwm']:.1f} |"
        )
    print()
    labels = [probe["probe"] for probe in next(iter(windows.values()))["probes"][1:]]
    print("| Probe | " + " | ".join(f"`{name}`" for name in windows) + " |")
    print("| --- |" + " ---: |" * len(windows))
    quiet = [f"{data['probes'][0]['cpu_ms_per_s']:.2f} ms CPU/s" for data in windows.values()]
    print("| quiet daemon | " + " | ".join(quiet) + " |")
    for label in labels:
        cells = []
        for data in windows.values():
            probe = next(probe for probe in data["probes"] if probe["probe"] == label)
            if any(status >= 300 for status in probe["statuses"]):
                cells.append("n/a")
            else:
                cells.append(f"{probe['mean_ms']:.2f} ms, {probe['net_cpu_ms_per_op']:.2f} ms CPU")
        print(f"| {label} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()

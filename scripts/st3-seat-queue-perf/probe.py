#!/usr/bin/env python3
"""Measure daemon CPU and latency per request, and per reconcile trigger.

Usage: probe.py SOCKET PID SEAT...

Run it after the idle window, with no other traffic. Each probe reports mean
and p95 latency plus the daemon's CPU time per operation. CPU comes from
/proc/PID/stat, so it includes everything the request caused in the daemon,
such as the reconcile loop that a claim write wakes.
"""

import http.client
import json
import os
import socket
import sys
import time
import urllib.parse

TICKS = os.sysconf("SC_CLK_TCK")


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.path)


def cpu_seconds(pid):
    with open(f"/proc/{pid}/stat") as stat:
        fields = stat.read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / TICKS


def request(connection, method, path, body=None):
    headers = {"content-type": "application/json"} if body else {}
    connection.request(method, path, body, headers)
    response = connection.getresponse()
    response.read()
    return response.status


def probe(label, pid, count, operation, pause=0.0):
    latencies = []
    statuses = set()
    before = cpu_seconds(pid)
    wall = time.perf_counter()
    for number in range(count):
        started = time.perf_counter()
        statuses.add(operation(number))
        latencies.append(time.perf_counter() - started)
        if pause:
            time.sleep(pause)
    if pause:
        # Let the last reconcile loop finish before reading CPU.
        time.sleep(2.0)
    spent = cpu_seconds(pid) - before
    wall = time.perf_counter() - wall
    latencies.sort()
    return {
        "probe": label,
        "count": count,
        "statuses": sorted(statuses),
        "mean_ms": round(sum(latencies) / count * 1000, 3),
        "p95_ms": round(latencies[int((count - 1) * 0.95)] * 1000, 3),
        "cpu_ms_per_op": round(spent / count * 1000, 3),
        "wall_s": round(wall, 3),
        "cpu_s": round(spent, 3),
    }


def main():
    path, pid, seats = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
    connection = UnixConnection(path)
    quoted = [urllib.parse.quote(seat, safe="") for seat in seats]
    results = []

    time.sleep(5.0)
    before = cpu_seconds(pid)
    time.sleep(30.0)
    results.append({"probe": "quiet daemon", "cpu_ms_per_s": round((cpu_seconds(pid) - before) / 30 * 1000, 3)})

    results.append(probe("roster read", pid, 200, lambda n: request(connection, "GET", "/v1/client/agents")))
    results.append(probe("work list for one seat", pid, 200,
                         lambda n: request(connection, "GET", f"/v1/work?actor={quoted[n % len(seats)]}")))
    results.append(probe("mailbox page for one seat", pid, 200,
                         lambda n: request(connection, "GET",
                                           f"/v1/messages/page?include_closed=false&limit=100&to={quoted[n % len(seats)]}")))
    results.append(probe("agent queue for one seat", pid, 200,
                         lambda n: request(connection, "GET", f"/v1/client/agent-queues/{quoted[n % len(seats)]}")))

    def claim(number):
        seat = seats[number % len(seats)]
        body = json.dumps({
            "subject": seat,
            "kind": "harness.timeline",
            "actor": seat,
            "fields": {
                "body": {"media_type": "text/plain", "text": f"probe entry {number}"},
                "driver": "claude",
                "entry_id": f"timeline-entry/probe-{os.getpid()}-{number:06d}",
                "entry_type": "content",
                "final": True,
                "incarnation_id": "probe:2026-01-01T00:00:00.000Z",
                "observed_at_unix_ms": 1_800_000_000_000 + number,
                "operation": "append",
                "revision": 1,
                "role": "assistant",
                "sequence": number + 1,
            },
            "evidence": [],
            "expected_subject": None,
            "idempotency_key": f"probe:{os.getpid()}:{number}",
        })
        return request(connection, "POST", "/v1/claims", body)

    # Spaced writes: each one wakes the reconciler, which then runs to quiet.
    results.append(probe("claim write and the reconcile it wakes", pid, 200, claim, pause=0.25))

    # Remove the quiet daemon's own CPU over each probe's wall time.
    quiet = results[0]["cpu_ms_per_s"]
    for result in results[1:]:
        result["net_cpu_ms_per_op"] = round(
            (result["cpu_s"] * 1000 - quiet * result["wall_s"]) / result["count"], 3
        )
    print(json.dumps(results))


if __name__ == "__main__":
    main()

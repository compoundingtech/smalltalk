#!/usr/bin/env python3
"""Send the requests idle native drivers send, for a fixed time.

Usage: traffic.py SOCKET SECONDS SEAT...

For each seat, a driver reads its mailbox page every second. Once a minute it
reads the seat's status and work list to renew held claims. The seats start
evenly spread over one second and keep those phases: each request goes at a
fixed time from the start, so late wakeups do not add up and move seats into
step with each other. Seats that send together make the daemon wake more, on
any build. The same schedule runs against both builds, so it adds the same load
to each. Set PERF_SEAT_SPREAD to a fraction of a second to spread the seats over
less than one second; 0 sends every seat's read at the same moment.

Set PERF_WRITES_PER_MINUTE to also publish that many harness timeline claims a
minute, spread over the seats, as idle drivers occasionally do. Each write
wakes the reconciler.
"""

import http.client
import json
import math
import os
import socket
import sys
import threading
import time
import urllib.parse


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.path)


def get(connection, path):
    connection.request("GET", path)
    response = connection.getresponse()
    response.read()
    return response.status


def ticks(first, period, clock, sleep):
    """Yield slot numbers at first + slot * period, skipping slots already missed.

    Each wait is measured from the start, not from the last wakeup, so the
    phase does not drift however late each sleep returns.
    """
    slot = 0
    while True:
        delay = first + slot * period - clock()
        if delay > 0:
            sleep(delay)
        yield slot
        slot = max(slot + 1, math.floor((clock() - first) / period) + 1)


def drive(path, seat, first, deadline, failures):
    connection = UnixConnection(path)
    quoted = urllib.parse.quote(seat, safe="")
    minute = None
    for tick in ticks(first, 1.0, time.monotonic, time.sleep):
        if time.monotonic() >= deadline:
            return
        try:
            statuses = [get(connection, f"/v1/messages/page?include_closed=false&limit=100&to={quoted}")]
            if tick // 60 != minute:
                minute = tick // 60
                statuses.append(get(connection, f"/v1/status?subject={quoted}"))
                statuses.append(get(connection, f"/v1/work?actor={quoted}"))
            if any(status >= 300 for status in statuses):
                failures.append((seat, statuses))
        except OSError as error:
            failures.append((seat, str(error)))
            connection = UnixConnection(path)


def write(path, seats, per_minute, deadline, failures):
    connection = UnixConnection(path)
    interval = 60.0 / per_minute
    number = 0
    next_write = time.monotonic() + interval
    while True:
        time.sleep(max(0.0, next_write - time.monotonic()))
        if time.monotonic() >= deadline:
            return
        seat = seats[number % len(seats)]
        body = json.dumps({
            "subject": seat,
            "kind": "harness.timeline",
            "actor": seat,
            "fields": {
                "body": {"media_type": "text/plain", "text": f"idle traffic entry {number}"},
                "driver": "claude",
                "entry_id": f"timeline-entry/idle-{os.getpid()}-{number:06d}",
                "entry_type": "content",
                "final": True,
                "incarnation_id": "idle:2026-01-01T00:00:00.000Z",
                "observed_at_unix_ms": 1_810_000_000_000 + number,
                "operation": "append",
                "revision": 1,
                "role": "assistant",
                "sequence": number + 1,
            },
            "evidence": [],
            "expected_subject": None,
            "idempotency_key": f"idle:{os.getpid()}:{number}",
        })
        try:
            connection.request("POST", "/v1/claims", body, {"content-type": "application/json"})
            response = connection.getresponse()
            response.read()
            if response.status >= 300:
                failures.append(("write", response.status))
        except OSError as error:
            failures.append(("write", str(error)))
            connection = UnixConnection(path)
        number += 1
        next_write += interval


def main():
    path, seconds, seats = sys.argv[1], float(sys.argv[2]), sys.argv[3:]
    spread = float(os.environ.get("PERF_SEAT_SPREAD", "1"))
    start = time.monotonic()
    deadline = start + seconds
    failures = []
    threads = [
        threading.Thread(
            target=drive, args=(path, seat, start + spread * index / len(seats), deadline, failures)
        )
        for index, seat in enumerate(seats)
    ]
    writes = float(os.environ.get("PERF_WRITES_PER_MINUTE", "0"))
    if writes > 0:
        threads.append(threading.Thread(target=write, args=(path, seats, writes, deadline, failures)))
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    if failures:
        print(f"{len(failures)} failed requests, first: {failures[0]}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()

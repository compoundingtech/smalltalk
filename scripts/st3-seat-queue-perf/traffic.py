#!/usr/bin/env python3
"""Send the requests idle native drivers send, for a fixed time.

Usage: traffic.py SOCKET SECONDS SEAT...

For each seat, a driver reads its mailbox page every second. Once a minute it
reads the seat's status and work list to renew held claims. The same schedule
runs against both builds, so it adds the same load to each.

Set PERF_WRITES_PER_MINUTE to also publish that many harness timeline claims a
minute, spread over the seats, as idle drivers occasionally do. Each write
wakes the reconciler.
"""

import http.client
import json
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


def drive(path, seat, offset, deadline, failures):
    connection = UnixConnection(path)
    quoted = urllib.parse.quote(seat, safe="")
    time.sleep(offset)
    tick = 0
    while time.monotonic() < deadline:
        started = time.monotonic()
        try:
            statuses = [get(connection, f"/v1/messages/page?include_closed=false&limit=100&to={quoted}")]
            if tick % 60 == 0:
                statuses.append(get(connection, f"/v1/status?subject={quoted}"))
                statuses.append(get(connection, f"/v1/work?actor={quoted}"))
            if any(status >= 300 for status in statuses):
                failures.append((seat, statuses))
        except OSError as error:
            failures.append((seat, str(error)))
            connection = UnixConnection(path)
        tick += 1
        time.sleep(max(0.0, 1.0 - (time.monotonic() - started)))


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
    deadline = time.monotonic() + seconds
    failures = []
    threads = [
        threading.Thread(target=drive, args=(path, seat, index / len(seats), deadline, failures))
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

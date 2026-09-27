#!/usr/bin/env python3
"""Append synthetic harness timeline claims, the most common claim kind in a live graph.

Usage: timeline.py SOCKET COUNT SEAT...

Claims are spread round-robin over the seats, as drivers publish them, and each
carries a short text body of realistic size.
"""

import http.client
import json
import socket
import sys


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost")
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.path)


def main():
    path, count, seats = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
    connection = UnixConnection(path)
    incarnations = {seat: f"perf-{index}:2026-01-01T00:00:00.000Z" for index, seat in enumerate(seats)}
    for number in range(count):
        seat = seats[number % len(seats)]
        sequence = number // len(seats) + 1
        fields = {
            "body": {
                "media_type": "text/plain",
                "text": f"Synthetic timeline entry {number}: the seat read its work and reported progress. " * 2,
            },
            "driver": "claude",
            "entry_id": f"timeline-entry/perf-{number:08d}",
            "entry_type": "content",
            "final": True,
            "incarnation_id": incarnations[seat],
            "observed_at_unix_ms": 1_790_000_000_000 + number,
            "operation": "append",
            "revision": 1,
            "role": "assistant",
            "sequence": sequence,
        }
        body = json.dumps({
            "subject": seat,
            "kind": "harness.timeline",
            "actor": seat,
            "fields": fields,
            "evidence": [],
            "expected_subject": None,
            "idempotency_key": f"perf-timeline:{number}",
        })
        connection.request("POST", "/v1/claims", body, {"content-type": "application/json"})
        response = connection.getresponse()
        payload = response.read()
        if response.status >= 300:
            sys.exit(f"claim {number} failed with {response.status}: {payload[:400]!r}")
        if number and number % 10_000 == 0:
            print(f"{number} timeline claims", flush=True)


if __name__ == "__main__":
    main()

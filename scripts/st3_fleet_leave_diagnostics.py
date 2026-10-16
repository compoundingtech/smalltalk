#!/usr/bin/env python3
"""Bound one fleet-leave fixture and dump evidence before its guardian cleans up."""
import argparse
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import time


def process_tree(root_pid):
    pending = [root_pid]
    seen = set()
    rows = []
    while pending:
        pid = pending.pop()
        if pid in seen:
            continue
        seen.add(pid)
        proc = Path(f"/proc/{pid}")
        try:
            args = proc.joinpath("cmdline").read_bytes().split(b"\0")
            # Show command/operation, never an invite or credential argument.
            row = {"pid": pid, "command": [a.decode(errors="replace") for a in args[:3]],
                   "status": proc.joinpath("status").read_text()}
            threads = []
            for thread in sorted(proc.joinpath("task").iterdir()):
                pending.extend(map(int, thread.joinpath("children").read_text().split()))
                threads.append({"tid": int(thread.name),
                                "name": thread.joinpath("comm").read_text().strip(),
                                "wait": thread.joinpath("wchan").read_text().strip()})
            row["threads"] = threads
            rows.append(row)
        except OSError as error:
            rows.append({"pid": pid, "snapshot_error": str(error)})
    return rows


def log_tail(path):
    with path.open("rb") as stream:
        stream.seek(0, os.SEEK_END)
        length = stream.tell()
        stream.seek(max(0, length - 65536))
        return stream.read().decode(errors="replace")


def live_latency(path):
    deadline = time.monotonic() + 2
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(2)
        sock.connect(str(path))
        sock.settimeout(max(0.001, deadline - time.monotonic()))
        # TODO(live-migration: arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both): keep old names until all members accept aliases; sunset 60 days after first shipping release.
        sock.sendall(b"GET /v1/client/request-latency HTTP/1.1\r\nHost: localhost\r\n"
                     b"x-st3-person: person/fleet-tester\r\nConnection: close\r\n\r\n")
        sock.setblocking(False)
        body = bytearray()
        while len(body) < 262144:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([sock], [], [], remaining)[0]:
                return body.decode(errors="replace") + "\n(snapshot deadline reached)"
            chunk = sock.recv(min(65536, 262144 - len(body)))
            if not chunk:
                break
            body.extend(chunk)
        return body.decode(errors="replace")


def dump(root, reason):
    print(f"fleet-leave diagnostic failure: {reason}", file=sys.stderr, flush=True)
    for name in ("phase.txt", "command.json"):
        path = root / name
        print(f"--- {name}", file=sys.stderr)
        print(path.read_text() if path.exists() else "not yet recorded", file=sys.stderr)
    guardian = int(os.environ["SMALLTALK_TEST_SUPERVISOR"])
    print("--- owned process tree (before cleanup)", file=sys.stderr)
    print(json.dumps(process_tree(guardian), indent=2), file=sys.stderr, flush=True)
    for node in ("a", "b"):
        for name in ("daemon.log", "daemon.stderr.log", "worker.log", "worker.stderr.log"):
            path = root / node / name
            if path.exists():
                print(f"--- {node}/{name} (last 65536 bytes)", file=sys.stderr)
                print(log_tail(path), file=sys.stderr, flush=True)
        # This read has its own socket bound: a stuck API must not hide the dump.
        try:
            body = live_latency(root / node / "run/st3.sock")
            print(f"--- {node} live request-latency\n{body}",
                  file=sys.stderr, flush=True)
        except OSError as error:
            print(f"--- {node} live request-latency unavailable: {error}",
                  file=sys.stderr, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=float, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command or args.seconds <= 0:
        parser.error("a command and positive deadline are required")
    if "SMALLTALK_TEST_SUPERVISOR" not in os.environ:
        parser.error("must run inside st3_test_process.py so all children are owned")
    task = subprocess.Popen(command, env={**os.environ,
        "SMALLTALK_FLEET_LEAVE_DIAGNOSTICS": str(args.root)})
    with os.fdopen(os.pidfd_open(task.pid), "rb") as fd:
        poller = select.poll()
        poller.register(fd, select.POLLIN)
        if not poller.poll(round(args.seconds * 1000)):
            dump(args.root, f"test exceeded {args.seconds:g}s")
            # The guardian kills/reaps the still-live test and its complete tree.
            return 124
    code = task.wait()
    if code:
        dump(args.root, f"test exited with {code}")
    return code if code >= 0 else 128 - code


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
import json
import subprocess
import sys
import time


def status(socket):
    completed = subprocess.run(
        ["st3", "--endpoint", socket, "--json", "replication", "status"],
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        return None
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError:
        return None


left, right, timeout = sys.argv[1], sys.argv[2], float(sys.argv[3])
deadline = time.monotonic() + timeout
while time.monotonic() < deadline:
    a = status(left)
    b = status(right)
    if (
        a
        and b
        and a["authority_digest"] == b["authority_digest"]
        and a["graph_digest"] == b["graph_digest"]
        and not a["pending_records"]
        and not b["pending_records"]
    ):
        print(a["authority_digest"])
        sys.exit(0)
    time.sleep(0.05)
print("the two stores did not converge before the deadline", file=sys.stderr)
sys.exit(1)

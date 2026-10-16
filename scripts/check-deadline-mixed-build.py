#!/usr/bin/env python3
"""Drive identical ignored probes built at an old and a new revision.

Build --lib deadline_mixed_build_probe --no-run at each revision after including
store/mission_deadline_probe.rs in store::tests, and copy each executable aside.
Pass OLD_EXE NEW_EXE. No daemon, real fleet or network is contacted.
"""
import json
import os
import pathlib
import subprocess
import sys
import tempfile

TEST = "store::tests::mission_deadline_probe::deadline_mixed_build_probe"
START = 1800000000000
EXPECT = sys.argv[3] if len(sys.argv) > 3 else "repeating"

class Member:
    def __init__(self, binary, origin, path):
        self.origin = origin
        self.process = subprocess.Popen([binary, "--exact", TEST, "--ignored", "--nocapture"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
            env={**os.environ, "DEADLINE_PROBE_ORIGIN": origin, "DEADLINE_PROBE_DB": str(path)})
        self.capabilities = self.read()
        assert self.capabilities["ready"]
    def read(self):
        while line := self.process.stdout.readline():
            if line.startswith("PROBE "):
                return json.loads(line[6:])
        raise RuntimeError(f"{self.origin} exited {self.process.wait()}")
    def call(self, op, **fields):
        self.process.stdin.write(json.dumps({"op": op, **fields}) + "\n")
        self.process.stdin.flush()
        return self.read()
    def close(self):
        self.process.stdin.write('{"op":"quit"}\n')
        self.process.stdin.flush()
        assert self.process.wait(timeout=30) == 0

def sync(source, target):
    exchange = source.call("export", inventory=target.call("inventory"))
    target.call("receive", exchange=exchange)

def heal(asker, peer):
    query = "ranges"
    for _ in range(48):
        answer = peer.call("answer", peer=asker.origin, query=query)
        step = asker.call("next", peer=peer.origin, answer=answer)
        if "done" in step:
            return step["done"]["report"]
        query = step["ask"]["query"]
    raise AssertionError("heal did not finish")

def replay(member, peer):
    if member.capabilities.get("local_replay"):
        member.call("replay")
    else:
        member.call("answer", peer=peer.origin, query="replay")

def compare(asker, peer):
    return asker.call("compare", exchange=peer.call("summary"))

with tempfile.TemporaryDirectory(prefix="deadline-mixed-") as directory:
    root = pathlib.Path(directory)
    old = Member(sys.argv[1], "old", root / "old.sqlite3")
    new = Member(sys.argv[2], "new", root / "new.sqlite3")
    for member in [old, new]:
        member.call("clock", at=START)
    old.call("create")
    # Canonicalize the old member through its real replay route, so local writer wall time
    # cannot introduce unrelated differences against accepted-time replica folds.
    replay(old, new)
    sync(old, new)
    left, right = old.call("summary"), new.call("summary")
    assert left["inventory"]["digest"] == right["inventory"]["digest"]
    assert (left["schema_digest"] == right["schema_digest"]) == (EXPECT != "fenced")
    if EXPECT == "fenced":
        assert right["projection_registry_digest"] == left["schema_digest"]
        assert right["projection_versions"]["mission_run_deadlines"] == 3
        assert left.get("projection_versions", {}).get("mission_run_deadlines", 1) == 1
    differing = [key for key in left["projection_digests"]
                 if left["projection_digests"][key] != right["projection_digests"].get(key)]
    print(json.dumps({"phase": "queued", "different_tables": differing,
        "old_deadline": old.call("view").get("deadline_at_unix_ms"),
        "new_deadline": new.call("view").get("deadline_at_unix_ms")}), flush=True)
    assert ("mission_run_deadlines" in differing) == (EXPECT != "same"), differing
    # Keep the old member on its build for three days of actual exchange comparisons.
    # An old build can still schedule sync heal; the new build has no such route.
    count = {"old": 0, "new": 0}
    for offset in [0, 60001, 120002, 1200000, 7200000, 86400000, 172800000, 259200000]:
        for member in [old, new]:
            member.call("clock", at=START + offset)
        for asker, peer in [(old, new), (new, old)]:
            receipt = compare(asker, peer)
            if not asker.capabilities.get("sync_heal", True):
                assert "heal" not in receipt, receipt
            if receipt.get("heal", False):
                report = heal(asker, peer)
                count[asker.origin] += 1
                print(json.dumps({"lag_ms": offset, "asker": asker.origin, "report": report}), flush=True)
    if EXPECT == "repeating":
        assert all(value > 1 for value in count.values()), count
    else:
        assert all(value == 0 for value in count.values()), count
    after_left, after_right = old.call("summary"), new.call("summary")
    assert after_left["projection_digests"] == left["projection_digests"]
    assert after_right["projection_digests"] == right["projection_digests"]
    print(json.dumps({"repeating_heals": count, "projections_unchanged": True,
        "new_sync_heal": new.capabilities.get("sync_heal", True)}), flush=True)
    if EXPECT != "same":
        import sqlite3
        with sqlite3.connect(root / "new.sqlite3") as source_db, sqlite3.connect(root / "queued.sqlite3") as queued_db:
            source_db.backup(queued_db)
        admitted_at = START + 4 * 86400000 + 1
        for member in [old, new]:
            member.call("clock", at=admitted_at)
        new.call("admit")
        replay(new, old)
        sync(new, old)
        admitted = new.call("view").get("deadline_at_unix_ms")
        legacy = old.call("view").get("deadline_at_unix_ms")
        print(json.dumps({"phase": "admitted", "old_deadline": legacy, "new_deadline": admitted}), flush=True)
        assert admitted == admitted_at + 300000 and legacy != admitted
    # Read this build's nullable database with the actual older executable.
    old.close()
    new.close()
    if EXPECT == "same":
        sys.exit(0)
    rollback = Member(sys.argv[1], "old", root / "queued.sqlite3")
    error = rollback.call("deadline")
    print(json.dumps({"rollback_deadline_read": error}), flush=True)
    assert "error" in error and "Null" in error["error"], error
    rollback.close()

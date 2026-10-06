"""Retain existing token-free native fixture evidence after a failed test has ended."""
import fnmatch
import json
import os


def emit_failure_files(evidence):
    """Bound reads and raw retained bytes; JSON escaping can expand the output.

    Fixture collection itself may already have read complete logs. This only bounds
    the final stdout copy, after judging and cleanup, and performs no live queries.
    """
    # Keep the hook exit/stderr and observation seam before verbose driver logs.
    patterns = ("receipts*.jsonl", "*.receipts.jsonl", "observation-outboxes.json",
                "harness-state-*.json", "trace*.json*", "*.claims.json",
                "driver-*.log", "daemon.log", "terminal*.txt")
    budget = 256 * 1024
    half = 8 * 1024
    def priority(path):
        return (next((i for i, pattern in enumerate(patterns)
                      if fnmatch.fnmatchcase(path.name, pattern)), len(patterns)), path.name)

    for path in sorted(evidence.iterdir(), key=priority):
        if not any(fnmatch.fnmatchcase(path.name, pattern) for pattern in patterns):
            continue
        try:
            with path.open("rb") as stream:
                size = os.fstat(stream.fileno()).st_size
                head = stream.read(min(size, half * 2, budget))
                tail = b""
                if size > len(head) and budget > len(head):
                    head = head[:half]
                    stream.seek(max(len(head), size - min(half, budget - len(head))))
                    tail = stream.read(min(half, budget - len(head)))
            budget -= len(head) + len(tail)
            print(json.dumps({"native_failure_evidence": path.name, "file_bytes": size,
                              "retained_bytes": len(head) + len(tail),
                              "omitted_bytes": size - len(head) - len(tail),
                              "head": head.decode(errors="replace"),
                              "tail": tail.decode(errors="replace")}), flush=True)
        except OSError as error:
            print(json.dumps({"native_failure_evidence": path.name,
                              "capture_error": str(error)}), flush=True)
        if budget <= 0:
            print(json.dumps({"native_failure_evidence": "byte budget exhausted"}), flush=True)
            return

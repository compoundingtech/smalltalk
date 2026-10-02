"""What a model does when st wakes it, shared by the token-free provider stand-ins.

A woken seat is told to run `st work claim STEP` and may read the message. The stand-ins do both
through the real st CLI with the seat's own environment, so a canary proves the whole boot path:
daemon, driver, delivery, and a seat that can act on what it received. Every action is appended to
receipts-<seat>.jsonl in the workspace.
"""
import json
import os
from pathlib import Path
import re
import subprocess
import time


def seat():
    return os.environ.get("ST_AGENT", "unknown")


def receipt(event, **fields):
    name = re.sub(r"[^A-Za-z0-9]", "-", seat())
    with (Path.cwd() / f"receipts-{name}.jsonl").open("a") as stream:
        stream.write(json.dumps({"event": event, "at": time.time(), "pid": os.getpid(), **fields}) + "\n")


def st(*args):
    result = subprocess.run([os.environ["ST3_BIN"], *args], capture_output=True, text=True, timeout=60)
    return result


def act(content):
    """Read the message and claim the step it names, as a model following the wake would."""
    message = re.search(r"graph=\"(message/[0-9a-f]+)\"", content) or re.search(r"message/[0-9a-f]+", content)
    step = re.search(r"st work claim (step-run/[^\s`]+)", content)
    if message:
        reference = message.group(1) if message.groups() else message.group(0)
        result = st("conversations", "read", reference, "--as", seat(), "--json")
        receipt("read", reference=reference, exit=result.returncode, stderr=result.stderr[-1000:])
    if step:
        result = st("work", "claim", step.group(1), "--as", seat())
        receipt("claim", step=step.group(1), exit=result.returncode, stderr=result.stderr[-1000:])

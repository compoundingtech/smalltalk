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
import shlex
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
    """Read the message, then run the command the wake tells a model to run, exactly as written.

    The stand-in adds nothing the text leaves out: a wake a model cannot follow verbatim fails the
    canary, as it fails a real seat (a work action needs `--as`)."""
    message = re.search(r"graph=\"(message/[0-9a-f]+)\"", content) or re.search(r"message/[0-9a-f]+", content)
    command = re.search(r"Run `(st work claim [^`]+)`", content)
    if message:
        reference = message.group(1) if message.groups() else message.group(0)
        result = st("conversations", "read", reference, "--as", seat(), "--json")
        receipt("read", reference=reference, exit=result.returncode, stderr=result.stderr[-1000:])
    if command:
        argv = shlex.split(command.group(1))[1:]
        result = st(*argv)
        receipt("claim", command=command.group(1), exit=result.returncode, stderr=result.stderr[-1000:])

#!/usr/bin/env python3
"""Script-controlled Claude stand-in; real driver hooks, native JSONL, and PTY output."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import uuid


def option(name):
    for i, arg in enumerate(sys.argv):
        if arg == name:
            return sys.argv[i + 1]
        if arg.startswith(name + "="):
            return arg.split("=", 1)[1]
    return None


if sys.argv[1:2] in (["plugin"], ["mcp"]) or "--version" in sys.argv:
    sys.exit(1)
settings = json.loads(option("--settings") or "{}")
root = Path(os.environ["HOME"]).parent
script = json.loads((root / "script.json").read_text())
session = "example-atlas-builder"
cwd = str(Path.cwd())
projects = Path.home() / ".claude/projects" / re.sub(r"[^A-Za-z0-9]", "-", cwd)
projects.mkdir(parents=True, exist_ok=True)
transcript = projects / (session + ".jsonl")


def hook(event, **payload):
    base = dict(session_id=session, transcript_path=str(transcript), cwd=cwd, hook_event_name=event)
    for group in settings.get("hooks", {}).get(event, []):
        for item in group.get("hooks", []):
            subprocess.run(["bash", "-c", item["command"]], input=json.dumps({**base, **payload}),
                           text=True, capture_output=True, check=True, timeout=60)


def append(role, text, timestamp):
    content = text if role == "user" else [{"type": "text", "text": text}]
    with transcript.open("a") as stream:
        stream.write(json.dumps(dict(type=role, uuid=str(uuid.uuid4()), timestamp=timestamp,
                                     sessionId=session, cwd=cwd, message=dict(role=role, content=content))) + "\n")


hook("SessionStart", source="startup", model="claude-example")
# Like a native TUI, paint the initial screen rather than retaining launcher scrollback.
print("\033[2J\033[Hscenario harness ready", flush=True)
(root / "ready").touch()
for index, turn in enumerate(script["turns"]):
    while not (root / f"step-{index}").exists():
        time.sleep(0.02)
    hook("UserPromptSubmit", prompt=turn["prompt"], permission_mode="default")
    append("user", turn["prompt"], turn["timestamp"])
    command = turn.get("command")
    if command:
        result = subprocess.run(["/bin/sh", "-c", command], capture_output=True, check=True)
        sys.stdout.buffer.write(result.stdout)
        sys.stdout.buffer.flush()
        (root / f"output-{index}").write_bytes(result.stdout)
        # A projected per-turn marker fences this command, not a previous screen change.
        print(f"\nscenario turn {index} output complete", flush=True)
    append("assistant", turn["answer"], turn["timestamp"])
    hook("Stop", stop_hook_active=False, last_assistant_message=turn["answer"])
    (root / f"done-{index}").touch()
while True:
    time.sleep(1)

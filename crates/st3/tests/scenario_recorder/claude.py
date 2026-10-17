#!/usr/bin/env python3
"""Script-controlled Claude stand-in; real driver hooks, native JSONL, and PTY output."""
from datetime import datetime, timezone
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
            if i + 1 >= len(sys.argv):
                raise ValueError(f"missing value for {name}")
            return sys.argv[i + 1]
        if arg.startswith(name + "="):
            return arg.split("=", 1)[1]
    return None


if sys.argv[1:2] in (["plugin"], ["mcp"]) or "--version" in sys.argv:
    sys.exit(1)
settings = json.loads(option("--settings") or "{}")
root = Path(os.environ["HOME"]).parent
script = json.loads((root / "script.json").read_text())
agent = os.environ["ST_AGENT"]
cast = [script["agent"]]
for seat in script.get("agents", []):
    if seat["host"] != script["host"]:
        raise ValueError("refusing another host's recorded agent")
    if seat["agent"] not in cast:
        cast.append(seat["agent"])
seat_index = cast.index(agent)
session = "example-atlas-builder" if seat_index == 0 else f"example-scenario-seat-{seat_index}"
cwd = str(Path.cwd())
projects = Path.home() / ".claude/projects" / re.sub(r"[^A-Za-z0-9]", "-", cwd)
projects.mkdir(parents=True, exist_ok=True)
transcript = projects / (session + ".jsonl")
record_index = 0
current_timestamp = None


def hook(event, **payload):
    base = dict(session_id=session, transcript_path=str(transcript), cwd=cwd, hook_event_name=event)
    for group in settings.get("hooks", {}).get(event, []):
        for item in group.get("hooks", []):
            subprocess.run(["bash", "-c", item["command"]], input=json.dumps({**base, **payload}),
                           text=True, capture_output=True, check=True, timeout=60)


def append(role, content):
    # Version 2 receives its effective native stamp at release; version 1 uses the authored turn stamp.
    timestamp = current_timestamp or datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")
    global record_index
    record_id = uuid.uuid5(uuid.NAMESPACE_URL, f"st3-scenario/{session}/{record_index}")
    record = dict(type=role, uuid=str(record_id), timestamp=timestamp,
                  sessionId=session, cwd=cwd, message=dict(role=role, content=content))
    with transcript.open("a") as stream:
        stream.write(json.dumps(record) + "\n")
    record_index += 1
    return record


def revise(prefix, record, text):
    # Replace the last native record at the same line/UUID. Appending another record
    # would create another native sequence, not revise the projected entry.
    record["timestamp"] = current_timestamp or datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")
    record["message"]["content"] = [{"type": "text", "text": text}]
    replacement = transcript.with_suffix(".pending")
    replacement.write_bytes(prefix + (json.dumps(record) + "\n").encode())
    replacement.replace(transcript)


hook("SessionStart", source="startup", model="claude-example")
print("\033[2J\033[Hscenario harness ready", flush=True)
(root / f"ready-{seat_index}").touch()
for index, turn in enumerate(script["turns"]):
    if turn.get("agent", script["agent"]) != agent:
        continue
    while not (root / f"step-{index}").exists():
        time.sleep(0.02)
    if script["format_version"] == 2:
        current_timestamp = (root / f"timestamp-{index}").read_text()
        datetime.fromisoformat(current_timestamp.replace("Z", "+00:00"))
    else:
        current_timestamp = turn["timestamp"]
    hook("UserPromptSubmit", prompt=turn["prompt"], permission_mode="default")
    append("user", turn["prompt"])
    command = turn.get("command")
    if command is not None:
        if agent != script["agent"]:
            raise ValueError("only the primary seat owns terminal commands")
        result = subprocess.run(["/bin/sh", "-c", command], capture_output=True, check=True, timeout=60)
        sys.stdout.buffer.write(result.stdout)
        sys.stdout.buffer.flush()
        (root / f"output-{index}").write_bytes(result.stdout)
        print(f"\nscenario turn {index} output complete", flush=True)
    tool = turn.get("tool")
    if tool is not None:
        hook("PreToolUse", tool_use_id=tool["id"], tool_name=tool["name"], tool_input=tool["input"])
        append("assistant", [{"type": "tool_use", "id": tool["id"], "name": tool["name"], "input": tool["input"]}])
        content = tool["result"] if isinstance(tool["result"], str) else json.dumps(tool["result"])
        append("user", [{"type": "tool_result", "tool_use_id": tool["id"], "content": content}])
        hook("PostToolUse", tool_use_id=tool["id"], tool_name=tool["name"],
             tool_input=tool["input"], tool_response=tool["result"])
    revisions = turn.get("revisions", [])
    if revisions:
        prefix = transcript.read_bytes()
        record = append("assistant", [{"type": "text", "text": revisions[0]}])
        for revision, text in enumerate(revisions):
            if revision:
                revise(prefix, record, text)
            (root / f"revision-{index}-{revision}").touch()
            deadline = time.monotonic() + 60
            while not (root / f"ack-{index}-{revision}").exists():
                if time.monotonic() >= deadline:
                    raise TimeoutError("native content projection acknowledgement timed out")
                time.sleep(0.02)
        revise(prefix, record, turn["answer"])
    else:
        append("assistant", [{"type": "text", "text": turn["answer"]}])
    hook("Stop", stop_hook_active=False, last_assistant_message=turn["answer"])
    (root / f"done-{index}").touch()
while True:
    time.sleep(1)

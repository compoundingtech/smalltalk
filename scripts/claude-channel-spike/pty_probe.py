#!/usr/bin/env python3
"""Run inside the disposable container; read a credential only from stdin."""
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import termios
import time

case, *args = sys.argv[1:]
key = sys.stdin.readline().strip()
assert key, "a runtime credential is required"
home_case = case.removesuffix("-retry")
home = Path("/home/ada") / ("home-" + home_case if case.startswith(("inline", "server")) else "home-installed")
home.mkdir(exist_ok=True)
config = home / ".claude" / ".claude.json"
config.parent.mkdir(exist_ok=True)
state = json.loads(config.read_text()) if config.exists() else {}
state.update({"hasCompletedOnboarding": True, "theme": "dark",
              "projects": {"/home/ada/work": {"hasTrustDialogAccepted": True}}})
config.write_text(json.dumps(state))
settings = {"apiKeyHelper": "python3 /opt/spike/key_helper.py",
            "autoUpdatesChannel": "stable", "includeCoAuthoredBy": False}
command = ["claude", "--debug-file", str(Path("/home/ada/results") / (case + "-debug.txt")), "--settings", json.dumps(settings), "--model", "sonnet",
           "--dangerously-skip-permissions", *args]
root = Path("/home/ada/results")
root.mkdir(exist_ok=True)
pid, fd = pty.fork()
if pid == 0:
    os.chdir("/home/ada/work")
    os.environ.update(HOME=str(home), CLAUDE_CONFIG_DIR=str(home / ".claude"),
                      SPIKE_API_KEY=key, SPIKE_CASE=case, TERM="xterm-256color",
                      DISABLE_AUTOUPDATER="1")
    os.execvp(command[0], command)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
ansi = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)")
raw = bytearray()
answers = []
handled = set()
debug_path = root / (case + "-debug.txt")
deadline = time.monotonic() + float(os.environ.get("SPIKE_TIMEOUT", "65"))
while time.monotonic() < deadline:
    if select.select([fd], [], [], 0.25)[0]:
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        raw.extend(chunk)
        screen = ansi.sub("", raw.decode(errors="replace"))
        (root / (case + "-terminal.txt")).write_text(screen.replace(key, "[REDACTED]"))
        compact = re.sub(r"\s+", "", screen)
        for label, needle, answer in [
            ("development", "I am using this for local development", b"\r"),
            ("trust", "Yes, I trust this folder", b"\r"),
            ("permissions", "Yes, I accept", b"\x1b[B\r"),
        ]:
            if label not in handled and re.sub(r"\s+", "", needle) in compact:
                time.sleep(0.6)
                os.write(fd, answer)
                handled.add(label)
                answers.append(label)
    skipped = debug_path.exists() and "Channel notifications skipped:" in debug_path.read_text()
    unavailable = "Channels are not currently available" in ansi.sub("", raw.decode(errors="replace"))
    if (skipped or unavailable) and (root / (case + "-sent")).exists():
        time.sleep(2)
        break
    if (root / (case + "-ack")).exists():
        time.sleep(2)
        break
try:
    os.killpg(pid, signal.SIGTERM)
except ProcessLookupError:
    pass
try:
    deadline = time.monotonic() + 3
    while os.waitpid(pid, os.WNOHANG)[0] == 0:
        if time.monotonic() >= deadline:
            os.killpg(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
            break
        time.sleep(0.1)
except ChildProcessError:
    pass
os.close(fd)
output = ansi.sub("", raw.decode(errors="replace")).replace(key, "[REDACTED]")
(root / (case + "-terminal.txt")).write_text(output)
summary = {"case": case, "argv": command, "answers": answers,
           "sent": (root / (case + "-sent")).exists(),
           "ack": (root / (case + "-ack")).read_text().strip() if (root / (case + "-ack")).exists() else None}
(root / (case + "-summary.json")).write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary))

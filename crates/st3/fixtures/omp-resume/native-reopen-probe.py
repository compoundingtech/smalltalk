#!/usr/bin/env python3
# LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
"""Model-free native OMP resume probe. Never sends F5, prompts, or any operator keys.

Copy this script and native-interrupted-ask.jsonl verbatim into dotfiles' OMP pin
(flakes/external/omp). That pin's hermetic flake check is the contraction gate on
EVERY OMP bump. smalltalk's Rust test is opt-in because it does not provide OMP.

OMP_BIN must name the raw pinned OMP executable, not a fleet extension launcher.
Exit 0: F5-to-retry is rendered, no native reopen (bridge still needed).
Exit 1: the original picker reopens natively (contract the bridge).
Exit 2: probe error, including a missing OMP_BIN or no observable retry UI.
"""
import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

MIGRATION = "arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge"
ANSI = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]")


def probe(args):
    if not args.omp:
        raise RuntimeError("OMP_BIN must name a raw pinned OMP executable")
    binary = str(Path(args.omp).resolve(strict=True))
    original = [json.loads(line) for line in args.fixture.read_text().splitlines()]
    header = next(row for row in original if row["type"] == "session")
    session_id = header["id"]
    ask_id = next(row["data"]["toolCallId"] for row in original
                  if row.get("customType") == "tool_execution_start")
    starts_before = sum(row.get("customType") == "tool_execution_start" and
                        row.get("data", {}).get("toolCallId") == ask_id for row in original)
    with tempfile.TemporaryDirectory(prefix="omp-native-reopen-") as temporary:
        root = Path(temporary)
        environment = {"PATH": os.environ.get("PATH", os.defpath), "TERM": "xterm-256color",
                       "PI_OFFLINE": "1", "NO_COLOR": "1"}
        for name in ["HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME",
                     "XDG_DATA_HOME", "XDG_RUNTIME_DIR"]:
            directory = root / name.lower()
            directory.mkdir()
            environment[name] = str(directory)
        sessions = root / "sessions"
        sessions.mkdir()
        # Header cwd is deliberately absent from the public fixture. Supply only this disposable cwd.
        header["cwd"] = str(root)
        transcript = sessions / f"2000-01-01T00-00-00-000Z_{session_id}.jsonl"
        transcript.write_text("".join(json.dumps(row) + "\n" for row in original))
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        child = subprocess.Popen([binary, "--session-dir", str(sessions), "--resume", session_id,
                                  "--no-extensions", "--no-skills", "--no-rules", "--no-lsp", "--no-title"],
                                 stdin=slave, stdout=slave, stderr=slave, cwd=root,
                                 env=environment, start_new_session=True)
        os.close(slave)
        output = bytearray()
        deadline = time.monotonic() + args.timeout
        retry_seen_at = None
        result = None
        try:
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.1)[0]:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                        break
                    if not chunk:
                        break
                    output.extend(chunk)
                    # Terminal-query replies only; these are not input keys or a model prompt.
                    if b"\x1b[6n" in chunk:
                        os.write(master, b"\x1b[1;1R")
                    if b"\x1b[c" in chunk:
                        os.write(master, b"\x1b[?1;2c")
                screen = ANSI.sub("", output.decode(errors="replace"))
                persisted = [json.loads(line) for line in transcript.read_text().splitlines()]
                starts = sum(row.get("customType") == "tool_execution_start" and
                             row.get("data", {}).get("toolCallId") == ask_id for row in persisted)
                if "Other (type your own)" in screen or starts > starts_before:
                    result = 1
                    break
                if re.search(r"F5\s+to\s+Retry", screen, re.IGNORECASE):
                    retry_seen_at = retry_seen_at or time.monotonic()
                if retry_seen_at and time.monotonic() - retry_seen_at >= 1:
                    result = 0
                    break
                if child.poll() is not None:
                    break
            screen = ANSI.sub("", output.decode(errors="replace"))
            if args.screen_out:
                args.screen_out.write_text(screen)
            if result is None:
                raise RuntimeError(f"no native picker or F5-to-retry UI before deadline; exit={child.poll()}\n{screen}")
            if result == 1:
                print(f"CONTRACT {MIGRATION}: OMP reopened {ask_id} natively without F5")
            else:
                print(f"BRIDGE STILL NEEDED {MIGRATION}: F5-to-retry rendered, no native reopen")
            return result
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
            os.close(master)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--omp", default=os.environ.get("OMP_BIN"))
    parser.add_argument("--fixture", type=Path, default=Path(__file__).with_name("native-interrupted-ask.jsonl"))
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--screen-out", type=Path)
    args = parser.parse_args()
    try:
        return probe(args)
    except Exception as error:
        print(f"PROBE ERROR {MIGRATION}: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
# LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge

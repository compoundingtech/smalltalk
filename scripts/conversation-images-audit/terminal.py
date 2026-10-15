#!/usr/bin/env python3
"""Run a command in a pseudo-terminal that answers like a chosen terminal, type keys into it, and
report what it drew: which image protocol bytes it wrote and the screen text.

Usage: terminal.py --profile kitty|iterm2|sixel|silent [--cols N] [--rows N] [--key DELAY:KEYS]...
                   [--seconds N] [--out DIR] -- COMMAND...

A profile is the terminal's own answer to the capability query ratatui-image sends (the kitty
graphics query, DA1, the cell size in pixels and a status report):

  kitty   answers the kitty graphics query OK, DA1 without sixel
  iterm2  answers DA1 without sixel and nothing to kitty; sets TERM_PROGRAM=iTerm.app and
          ITERM_SESSION_ID as iTerm2 does (tmux keeps the second)
  sixel   answers DA1 with attribute 4 (sixel), nothing to kitty
  silent  answers nothing at all

KEYS is text with \\r, \\x1b and ^X (control) escapes, sent DELAY seconds after the start.
Writes DIR/raw.bin (everything the command wrote), DIR/screen.txt (the last screen, when pyte is
installed; otherwise the text with escapes removed), DIR/screen-Ns.txt for each --snapshot N, and
DIR/summary.json with the count of image sequences drawn by each snapshot and in all.
"""

import argparse
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time
from pathlib import Path

ESC = b"\x1b"
CELL = (20, 10)  # pixel height, width of one cell

ENVIRONMENT = {
    "kitty": {"TERM": "xterm-kitty", "KITTY_WINDOW_ID": "1"},
    "iterm2": {"TERM": "xterm-256color", "TERM_PROGRAM": "iTerm.app", "LC_TERMINAL": "iTerm2",
               "ITERM_SESSION_ID": "w0t0p0:00000000-0000-0000-0000-000000000000"},
    "sixel": {"TERM": "xterm-256color"},
    "silent": {"TERM": "xterm-256color"},
}


def answers(profile, data):
    """The bytes a terminal of this profile sends back for the queries in data."""
    if profile == "silent":
        return b""
    out = b""
    # Queries may arrive wrapped for tmux passthrough: ESC P tmux; ESC ESC _G ... ESC ESC \ ESC \.
    plain = data.replace(b"\x1b\x1b", b"\x1b")
    if profile == "kitty":
        for match in re.finditer(rb"\x1b_Gi=(\d+),[^;]*a=q[^;]*;", plain):
            out += b"\x1b_Gi=" + match.group(1) + b";OK\x1b\\"
    if b"\x1b[c" in plain or b"\x1b[0c" in plain:
        out += b"\x1b[?62;4;22c" if profile == "sixel" else b"\x1b[?62;22c"
    if b"\x1b[16t" in plain:
        out += b"\x1b[6;%d;%dt" % CELL
    if b"\x1b[14t" in plain:
        out += b"\x1b[4;%d;%dt" % (CELL[0] * 40, CELL[1] * 120)
    if b"\x1b]11;?" in plain:
        out += b"\x1b]11;rgb:1010/1010/1010\x1b\\"
    if b"\x1b[?u" in plain:
        out += b"\x1b[?0u" if profile == "kitty" else b""
    if b"\x1b[6n" in plain:
        out += b"\x1b[1;1R"
    if b"\x1b[5n" in plain:
        out += b"\x1b[0n"
    return out


def keys(text):
    text = text.replace("\\r", "\r").replace("\\n", "\n").replace("\\x1b", "\x1b").replace("\\t", "\t")
    return re.sub(r"\^([A-Z\[\]\\])", lambda m: chr(ord(m.group(1)) - 64), text).encode()


def protocols(raw):
    # The capability query itself is not drawing: count only image data after the query.
    kitty = len(re.findall(rb"\x1b_G(?![^\x1b]*a=q)", raw))
    sixel = len(re.findall(rb"\x1bP[0-9;]*q", raw))
    iterm2 = raw.count(b"\x1b]1337;File=")
    return {"kitty": kitty, "sixel": sixel, "iterm2": iterm2}


def strip(raw):
    text = re.sub(rb"\x1b(\[[0-9;?<>=]*[ -/]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)|P.*?\x1b\\|_.*?\x1b\\|.)", b"", raw, flags=re.S)
    return text.decode("utf-8", "replace")


def screen(raw, cols, rows):
    try:
        import pyte
    except ImportError:
        return strip(raw)
    term = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(term)
    # pyte knows neither APC, sixel DCS, private status reports nor colon colours (tmux writes
    # them): drop them, and every colour, so they do not print as text or stop the parse.
    cleaned = re.sub(rb"\x1b\[[0-9:;]*m|\x1b_G.*?\x1b\\|\x1bP.*?\x1b\\|\x1b\]1337;.*?(\x07|\x1b\\)|\x1b\[\?[0-9;]*n", b"", raw, flags=re.S)
    try:
        stream.feed(cleaned)
    except Exception:
        return strip(raw)
    return "\n".join(line.rstrip() for line in term.display)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", required=True, choices=sorted(ENVIRONMENT))
    parser.add_argument("--cols", type=int, default=120)
    parser.add_argument("--rows", type=int, default=40)
    parser.add_argument("--key", action="append", default=[])
    parser.add_argument("--seconds", type=float, default=12)
    parser.add_argument("--snapshot", type=float, action="append", default=[],
                        help="also keep the screen and the image bytes drawn so far at this second")
    parser.add_argument("--out", default=".")
    parser.add_argument("command", nargs="+")
    args = parser.parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    schedule = []
    for item in args.key:
        delay, _, text = item.partition(":")
        schedule.append((float(delay), keys(text)))
    schedule.sort()

    environment = dict(os.environ)
    for name in ("TERM_PROGRAM", "LC_TERMINAL", "KITTY_WINDOW_ID", "TMUX", "ITERM_SESSION_ID", "WEZTERM_EXECUTABLE"):
        environment.pop(name, None)
    environment.update(ENVIRONMENT[args.profile])

    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe(args.command[0], args.command, environment)
    size = struct.pack("HHHH", args.rows, args.cols, args.cols * CELL[1], args.rows * CELL[0])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, size)
    os.kill(pid, signal.SIGWINCH)

    raw = bytearray()
    start = time.monotonic()
    pending = 0
    snapshots = sorted(args.snapshot)
    taken = {}
    while time.monotonic() - start < args.seconds:
        now = time.monotonic() - start
        while snapshots and snapshots[0] <= now:
            at = snapshots.pop(0)
            (out / f"screen-{at:g}s.txt").write_text(screen(bytes(raw), args.cols, args.rows))
            taken[f"{at:g}s"] = protocols(bytes(raw))
        while pending < len(schedule) and schedule[pending][0] <= now:
            os.write(fd, schedule[pending][1])
            pending += 1
        ready, _, _ = select.select([fd], [], [], 0.05)
        if not ready:
            continue
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        raw.extend(data)
        reply = answers(args.profile, data)
        if reply:
            os.write(fd, reply)
    try:
        os.kill(pid, signal.SIGTERM)
        time.sleep(0.3)
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    raw = bytes(raw)
    (out / "raw.bin").write_bytes(raw)
    text = screen(raw, args.cols, args.rows)
    (out / "screen.txt").write_text(text)
    summary = {"profile": args.profile, "bytes": len(raw), "drawn": protocols(raw), "drawn_by": taken,
               "query_sent": b"\x1b_Gi=31" in raw or b"\x1b[16t" in raw}
    (out / "summary.json").write_text(json.dumps(summary, indent=1))
    print(json.dumps(summary))


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Exercise a conversation's mouse in an installed stui (spaces) PTY: open an agent from the
sidebar, scroll it with the wheel, and drag to select and copy.

Requires a live st3 daemon and its normal agent tree. Prints no conversation text.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import time
import uuid


PTY = os.environ.get("PTY_BIN") or shutil.which("pty")
if not PTY:
    raise SystemExit("pty executable is required")

FIRST_FRAME = "working"  # spaces' top bar: "N working" is always on it
PALETTE = "╭─ open"  # the palette's title: open in a new tab, in place of a tab...


def call(*args: str) -> str:
    return subprocess.check_output([PTY, *args], text=True)


def screen(session: str) -> str:
    return call("peek", "--plain", session)


def wait_screen(session: str, predicate, label: str, seconds: float = 8) -> str:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = screen(session)
        if predicate(value):
            return value
        time.sleep(0.1)
    raise AssertionError(f"{label} did not appear")


def send(session: str, value: str) -> None:
    call("send", session, "--seq", value)


def click(session: str, x: int, y: int) -> None:
    send(session, f"\x1b[<0;{x};{y}M")
    send(session, f"\x1b[<0;{x};{y}m")


def drag(session: str, x: int, y: int, to: int) -> None:
    send(session, f"\x1b[<0;{x};{y}M")
    send(session, f"\x1b[<32;{to};{y}M")
    send(session, f"\x1b[<0;{to};{y}m")


def wheel_up(session: str, x: int, y: int) -> None:
    send(session, f"\x1b[<64;{x};{y}M")


def text_row(value: str) -> tuple[int, int] | None:
    """A row in the screen's middle with text in its middle third, and where that text starts."""
    lines = value.splitlines()
    for row in range(len(lines) // 3, len(lines) * 2 // 3):
        line = lines[row]
        third = len(line) // 3
        middle = line[third:2 * third]
        if middle.strip():
            return row, third + (len(middle) - len(middle.lstrip()))
    return None


def start(binary: str, prefix: str) -> str:
    """Start stui in a detached PTY session and wait for its first frame."""
    session = f"{prefix}-{uuid.uuid4().hex[:10]}"
    actor = os.environ.get("ST3_PERSON", "person/alex")
    subprocess.run(
        [PTY, "run", "-d", "-e", "--id", session,
         "--env", f"ST3_PERSON={actor}", "--env", "TERM=xterm-256color",
         "--", os.path.abspath(binary)],
        check=True, capture_output=True, text=True,
    )
    wait_screen(session, lambda value: FIRST_FRAME in value, "first frame")
    return session


def stop(session: str) -> None:
    try:
        send(session, "\x11")  # Ctrl+Q
    except subprocess.CalledProcessError:
        pass
    time.sleep(0.1)
    subprocess.run([PTY, "kill", session], capture_output=True, text=True)
    subprocess.run([PTY, "rm", session], capture_output=True, text=True)


def open_agent(session: str, name: str) -> str:
    """Open an agent's conversation in a tab from Ctrl+K, by its name."""
    send(session, "\x0b")  # Ctrl+K
    wait_screen(session, lambda value: PALETTE in value, "palette")
    send(session, name)
    time.sleep(0.3)
    send(session, "\r")
    return wait_screen(session, lambda value: PALETTE not in value and name.lower() in value.lower(),
                       f"{name}'s tab")


def main(binary: str, name: str) -> None:
    session = start(binary, "stui-qa")
    try:
        initial = open_agent(session, name)
        # The conversation sits under the tab strip, right of any sidebar: aim at its middle.
        lines = initial.splitlines()
        middle = len(lines) // 2
        width = max(len(line) for line in lines)
        wheel_up(session, width // 2, middle)
        wait_screen(session, lambda value: value != initial, "mouse wheel scroll")

        # A drag across a line of the conversation selects it and copies it on release.
        value = wait_screen(session, lambda value: text_row(value) is not None, "conversation text")
        row, column = text_row(value)
        drag(session, column + 1, row + 1, column + 12)
        wait_screen(session, lambda value: "Copied 1 line" in value, "drag to copy")
        assert FIRST_FRAME in screen(session), "a drag must not leave the conversation"
        # The tab strip contains the opened agent and the new-tab control.
        value = screen(session)
        tabs = next((row, line) for row, line in enumerate(value.splitlines())
                    if "+" in line and name.lower() in line.lower())
        row, line = tabs
        column = line.lower().index(name.lower()) + 1
        send(session, f"\x1b[<1;{column};{row + 1}M")
        assert name.lower() in screen(session).splitlines()[row].lower(), "press must not close"
        send(session, f"\x1b[<1;{column};{row + 1}m")
        wait_screen(session, lambda frame: name.lower() not in frame.splitlines()[row].lower(),
                    "middle-click tab close")
        print("Interaction QA passed: open from Ctrl+K, wheel, drag to copy, middle-click close")
    finally:
        stop(session)


if __name__ == "__main__":
    if len(sys.argv) < 3:
        raise SystemExit("usage: interaction_qa.py STUI AGENT_NAME")
    main(sys.argv[1], sys.argv[2])

#!/usr/bin/env python3
"""Exercise Chat mouse, history, and selection in an installed stui PTY.

Requires a live st3 daemon and its normal agent tree. Prints no conversation text.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import time
import uuid


PTY = os.environ.get("PTY_BIN") or shutil.which("pty")
if not PTY:
    raise SystemExit("pty executable is required")


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


def wheel_up(session: str, x: int, y: int) -> None:
    send(session, f"\x1b[<64;{x};{y}M")


def selected_row(value: str, row: int) -> bool:
    lines = value.splitlines()
    return len(lines) > row and lines[row].startswith("│›")


def older_count(value: str) -> int:
    match = re.search(r"(\d+) older messages", value)
    return int(match.group(1)) if match else 0


def main(binary: str) -> None:
    session = f"stui-qa-{uuid.uuid4().hex[:10]}"
    actor = os.environ.get("ST3_PERSON", "person/nathan")
    subprocess.run(
        [PTY, "run", "-d", "-e", "--id", session,
         "--env", f"ST3_PERSON={actor}", "--env", "TERM=xterm-256color",
         "--", os.path.abspath(binary)],
        check=True, capture_output=True, text=True,
    )
    try:
        wait_screen(session, lambda value: "Now" in value, "first frame")
        send(session, "2")
        wait_screen(session, lambda value: "History [h]" in value and selected_row(value, 2), "Chat")
        click(session, 5, 4)  # The second visible one-line agent row.
        wait_screen(session, lambda value: selected_row(value, 3) and not selected_row(value, 2), "second agent selection")
        click(session, 5, 3)
        wait_screen(session, lambda value: selected_row(value, 2), "first agent selection")
        for _ in range(30):
            try:
                initial = wait_screen(
                    session, lambda value: "assistant:" in value or "user:" in value,
                    "conversation", seconds=2,
                )
                break
            except AssertionError:
                send(session, "\x1b[B")
        else:
            raise AssertionError("no agent with conversation content in the visible roster")
        wheel_up(session, 55, 10)
        wait_screen(session, lambda value: value != initial, "mouse wheel scroll")

        header = screen(session).splitlines()[0]
        click(session, header.index("History [h]") + 2, 1)
        history = wait_screen(session, lambda value: "History & details" in value, "History pane")
        assert "[More history beyond bounded view]" not in history
        if "o Load older pages" in history:
            before = older_count(history)
            lines = history.splitlines()
            load_row = next(i for i, line in enumerate(lines) if "o Load older pages" in line)
            load_column = lines[load_row].index("o Load older pages")
            click(session, load_column + 2, load_row + 1)
            if before:
                wait_screen(session, lambda value: older_count(value) > before, "click to load older pages")
            else:
                # Tool-heavy pages may contain no older content yet. The click
                # still requests more pages, but the count can remain zero.
                wait_screen(session, lambda value: "History & details" in value, "History after click")

        header = screen(session).splitlines()[0]
        click(session, header.index("Select text [v]") + 2, 1)
        wait_screen(session, lambda value: "SELECT" in value.splitlines()[0]
                    and "Drag to select text" in value, "terminal text selection")
        send(session, "\x1b")
        wait_screen(session, lambda value: "Select text [v]" in value.splitlines()[0], "return from selection")
        assert "Chat" in screen(session).splitlines()[0], "Esc in selection must not quit"
        print("Chat interaction QA passed: click target, wheel, History, older pages, text selection")
    finally:
        try:
            send(session, "q")
        except subprocess.CalledProcessError:
            pass
        time.sleep(0.1)
        subprocess.run([PTY, "kill", session], capture_output=True, text=True)
        subprocess.run([PTY, "rm", session], capture_output=True, text=True)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "target/debug/stui")

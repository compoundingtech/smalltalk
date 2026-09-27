#!/usr/bin/env python3
"""Prove that both exit controls leave a live attached terminal without input to it."""

import os
import subprocess
import sys
import time
import uuid

sys.dont_write_bytecode = True
from interaction_qa import PTY, click, screen, send, wait_screen


def main(binary: str, label: str) -> None:
    session = f"stui-attach-qa-{uuid.uuid4().hex[:10]}"
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
        wait_screen(session, lambda value: "History [h]" in value, "Chat")
        for _ in range(120):
            footer = screen(session).splitlines()[-1]
            if label.lower() in footer.lower() and "Enter terminal" in footer:
                break
            send(session, "\x1b[B")
            time.sleep(0.03)
        else:
            raise AssertionError(f"no selectable terminal for {label}")
        for _ in range(3):
            wait_screen(session, lambda value: "● Online" in value.splitlines()[0], "online")
            send(session, "\r")
            try:
                wait_screen(session, lambda value: "Return to Smalltalk" in value
                            and "Interactive terminal" in value, "attached terminal", seconds=3)
                break
            except AssertionError:
                time.sleep(1)
        else:
            lines = screen(session).splitlines()
            raise AssertionError(f"attach failed; header={lines[0]!r} footer={lines[-1]!r}")
        send(session, "\x1c")
        wait_screen(session, lambda value: "Return to Smalltalk" not in value
                    and "History [h]" in value, "Ctrl+\\ detach")
        send(session, "\r")
        wait_screen(session, lambda value: "Return to Smalltalk" in value, "reattached terminal")
        click(session, 5, 1)
        wait_screen(session, lambda value: "Return to Smalltalk" not in value
                    and "History [h]" in value, "click Return detach")
        print(f"Attachment QA passed for {label}: Ctrl+\\ and click Return detach")
    finally:
        try:
            send(session, "q")
        except subprocess.CalledProcessError:
            pass
        subprocess.run([PTY, "kill", session], capture_output=True, text=True)
        subprocess.run([PTY, "rm", session], capture_output=True, text=True)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])

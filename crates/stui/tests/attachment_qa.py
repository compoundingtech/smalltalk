#!/usr/bin/env python3
"""Prove that both exit controls leave a live attached terminal without input to it: Ctrl+\\
and a click on the terminal's header line, in stui's spaces."""

import sys
import time

sys.dont_write_bytecode = True
from interaction_qa import click, open_agent, screen, send, start, stop, wait_screen

HEADER = "← Ctrl+\\"  # the attached terminal's header line


def attached(value: str) -> bool:
    return HEADER in value


def main(binary: str, name: str) -> None:
    session = start(binary, "stui-attach-qa")
    try:
        open_agent(session, name)
        for _ in range(3):
            send(session, "\x1d")  # Ctrl+]
            try:
                wait_screen(session, attached, "attached terminal", seconds=5)
                break
            except AssertionError:
                time.sleep(1)
        else:
            raise AssertionError(f"attach failed for {name}")
        send(session, "\x1c")  # Ctrl+\
        wait_screen(session, lambda value: not attached(value), "Ctrl+\\ detach")
        send(session, "\x1d")
        value = wait_screen(session, attached, "attached again")
        row = next(index for index, line in enumerate(value.splitlines()) if HEADER in line)
        column = value.splitlines()[row].index(HEADER)
        click(session, column + 2, row + 1)
        wait_screen(session, lambda value: not attached(value), "click on the header detaches")
        print(f"Attachment QA passed for {name}: Ctrl+\\ and a click on the header detach")
    finally:
        stop(session)


if __name__ == "__main__":
    if len(sys.argv) < 3:
        raise SystemExit("usage: attachment_qa.py STUI AGENT_NAME")
    main(sys.argv[1], sys.argv[2])

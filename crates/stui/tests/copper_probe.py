#!/usr/bin/env python3
"""Invented terminal program: emit requested bytes and log every byte read in raw mode.

Control uses files, so control traffic is never confused with terminal input. Output requests
can set any terminal protocol, make queries, or print fixtures. The JSONL log records output,
input and SIGWINCH geometry. There is no terminal window or network access.
"""
import fcntl
import json
import os
from pathlib import Path
import select
import signal
import struct
import sys
import termios
import time
import tty


def main(root):
    tty.setraw(0)
    log = open(root / "program.jsonl", "a", buffering=1)

    def record(kind, **data):
        log.write(json.dumps({"kind": kind, **data}) + "\n")

    def resized(*_):
        rows, cols, px, py = struct.unpack("HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))
        record("resize", rows=rows, cols=cols, pixels=[px, py])

    signal.signal(signal.SIGWINCH, resized)
    resized()
    current = None
    while not (root / "stop").exists():
        try:
            request = json.loads((root / "request.json").read_text())
        except (FileNotFoundError, json.JSONDecodeError):
            request = None
        if request and request["id"] != current:
            current = request["id"]
            output = bytes.fromhex(request["hex"])
            record("output", id=current, hex=output.hex())
            os.write(1, output + f"\x1b]0;copper-{current}\x07".encode())
        if select.select([0], [], [], 0.005)[0]:
            data = os.read(0, 65536)
            if not data:
                break
            record("input", hex=data.hex(), time=time.monotonic())
    log.close()


if __name__ == "__main__":
    main(Path(sys.argv[1]))

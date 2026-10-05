#!/usr/bin/env python3
"""Drive stui's actual terminal tab and a byte-logging program through two real PTYs.

Requires Python 3, the packaged `pty` daemon (CI provides both), and the stui unit-test
executable. `--check` compares the byte matrix with the committed baseline. `--record FILE`
records a new matrix for review. `--program NAME=PATH` additionally checks a real app against
an invented local document; no downloads, display server, real graph, or user's config.
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

HERE = Path(__file__).resolve().parent
WORKER = "ui::terminal_tab::terminal_tab_probe_worker"
ESC = b"\x1b"
BARRIER = b"\x02\x01\x02"  # Cannot be mistaken for a tilde at the end of a key sequence.
# Restore the main screen, default cursor mode and every mode used by the probe.
RESET = (b"\x1b[?1049l\x1b[?1l\x1b[?66l" + b"".join(
    f"\x1b[?{mode}l".encode() for mode in
    (1000, 1002, 1003, 1004, 1005, 1006, 1007, 1015, 1016, 2004, 2026, 2031, 2048, 5522)
) + b"\x1b[>4;0m\x1b[<u\x1b[0m\x1b[2J\x1b[H")


def wait_for(read, predicate, label, seconds=8):
    deadline = time.monotonic() + seconds
    value = None
    while time.monotonic() < deadline:
        value = read()
        if predicate(value):
            return value
        time.sleep(0.005)
    raise AssertionError(f"{label}: timed out; last={value!r}")


def json_file(path):
    try:
        return json.loads(path.read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def json_lines(path):
    try:
        lines = path.read_text().splitlines()
    except FileNotFoundError:
        return []
    return [json.loads(line) for line in lines if line.endswith("}")]


class Tab:
    def __init__(self, worker, command, directory):
        self.root = Path(directory)
        self.registry = self.root / "sessions"
        self.registry.mkdir()
        self.process = None
        self.master, self.slave = pty.openpty()
        self.resize(40, 120)
        self.env = {**os.environ, "TERM": "xterm-256color", "PTY_ROOT": str(self.registry),
                    "STUI_PROBE_ROOT": str(self.root), "STUI_PROBE_SOCKET": str(self.root / "tab.sock"),
                    "XDG_CONFIG_HOME": str(self.root / "config"),
                    "XDG_CACHE_HOME": str(self.root / "cache"),
                    "XDG_STATE_HOME": str(self.root / "state"), "HOME": str(self.root)}
        for name in ("ST_AGENT", "PTY_SESSION", "PTY_SESSION_DIR", "ST3_ENDPOINT", "ST3_PERSON"):
            self.env.pop(name, None)
        self.worker = worker
        self.command = command
        self.process = None
        self.output = bytearray()
        self.wire_input = bytearray()
        self.program_output = bytearray()
        self.relay_error = None
        self.stopping = threading.Event()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.root / "tab.sock"))
        self.listener.listen(1)
        self.listener.settimeout(10)
        self.request_id = 0

    def pty_command(self, *args, check=True):
        result = subprocess.run([shutil.which("pty") or "pty", *args], env=self.env,
                                cwd=self.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
        if check and result.returncode:
            raise AssertionError(f"pty {args[0]}: {result.stderr.decode(errors='replace')}")
        return result

    def start(self):
        self.pty_command("run", "--force", "-d", "--id", "copper", "--", *self.command)
        wait_for(lambda: (self.registry / "copper.sock").exists(), bool, "session socket")
        self.relay_thread = threading.Thread(target=self.relay, daemon=True)
        self.relay_thread.start()

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(
            [str(self.worker), "--exact", WORKER, "--ignored", "--nocapture"],
            stdin=self.slave, stdout=self.slave, stderr=self.slave, env=self.env,
            preexec_fn=controlling_terminal, cwd=self.root,
        )
        self.reader_thread = threading.Thread(target=self.drain, daemon=True)
        self.reader_thread.start()
        wait_for(self.status, lambda value: value and value.get("body"), "terminal tab")

    def drain(self):
        pending = b""
        while not self.stopping.is_set():
            if not select.select([self.master], [], [], 0.05)[0]:
                continue
            try:
                data = os.read(self.master, 65536)
            except OSError:
                return
            if not data:
                return
            self.output.extend(data)
            pending = (pending + data)[-65536:]
            # Answer only the outer terminal's capability handshake; the actual PTY daemon
            # answers the child's queries. Retain tails so split queries are handled too.
            if b"\x1b[?u" in pending:
                os.write(self.master, b"\x1b[?1u")
                pending = pending.replace(b"\x1b[?u", b"")
            if b"\x1b[c" in pending:
                os.write(self.master, b"\x1b[?62;22c")
                pending = pending.replace(b"\x1b[c", b"")

    def relay(self):
        # A transparent transport tap, not a substitute terminal/daemon: all packets go
        # unchanged to the real session socket. It observes what stui sends to real apps.
        try:
            client, _ = self.listener.accept()
            upstream = socket.socket(socket.AF_UNIX)
            upstream.connect(str(self.registry / "copper.sock"))
            with client, upstream:
                buffers = {client: bytearray(), upstream: bytearray()}
                while not self.stopping.is_set():
                    ready = select.select([client, upstream], [], [], 0.05)[0]
                    for source in ready:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is client else client).sendall(data)
                        buf = buffers[source]
                        buf.extend(data)
                        while len(buf) >= 5:
                            size = struct.unpack("!I", buf[1:5])[0]
                            if len(buf) < 5 + size:
                                break
                            if buf[0] == 0:
                                (self.wire_input if source is client else self.program_output).extend(buf[5:5+size])
                            del buf[:5+size]
        except Exception as error:
            if not self.stopping.is_set():
                self.relay_error = str(error)

    def resize(self, rows, cols):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        if self.process:
            self.process.send_signal(signal.SIGWINCH)

    def status(self):
        return json_file(self.root / "status.json")

    def input(self):
        return b"".join(bytes.fromhex(row["hex"]) for row in json_lines(self.root / "program.jsonl")
                        if row["kind"] == "input")

    def emit(self, data):
        self.request_id += 1
        request = {"id": self.request_id, "hex": data.hex()}
        (self.root / "request.new").write_text(json.dumps(request))
        (self.root / "request.new").replace(self.root / "request.json")
        return wait_for(self.status, lambda status: status and status["title"] == f"copper-{self.request_id}",
                        f"output {self.request_id}")

    def mouse(self, button, x=4, y=2, release=False):
        body = self.status()["body"]
        return f"\x1b[<{button};{body[0]+x+1};{body[1]+y+1}{'m' if release else 'M'}".encode()

    def send(self, data):
        os.write(self.master, data)

    def exercise(self, data):
        start = len(self.input())
        self.send(data + BARRIER)
        received = wait_for(self.input, lambda raw: len(raw) > start and raw.endswith(BARRIER),
                            f"input {data.hex()}")
        return received[start:-len(BARRIER)]

    def query(self, data):
        start = len(self.input())
        self.emit(data)
        self.send(BARRIER)
        received = wait_for(self.input, lambda raw: len(raw) > start and raw.endswith(BARRIER),
                            f"query {data.hex()}")
        return received[start:-len(BARRIER)]

    def close(self):
        (self.root / "stop").touch()
        if self.process:
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.stopping.set()
        self.listener.close()
        self.pty_command("kill", "copper", check=False)
        self.pty_command("rm", "copper", check=False)
        os.close(self.master)
        os.close(self.slave)


def matrix(tab):
    rows = []

    def case(name, sent, expected=None):
        received = tab.exercise(sent)
        if expected is not None:
            assert received == expected, f"{name}: expected {expected.hex()}, received {received.hex()}"
        rows.append({"case": name, "sent": sent.hex(), "received": received.hex()})

    # Every tracking protocol, encoding, screen and modifier combination; both directions.
    # A single packet per case includes all eight modifiers, keeping the CI run short.
    for alternate in (False, True):
        for tracking in (0, 1000, 1002, 1003):
            for encoding in (0, 1005, 1006, 1015):
                modes = RESET + (b"\x1b[?1049h" if alternate else b"")
                if tracking:
                    modes += f"\x1b[?{tracking}h".encode()
                if encoding:
                    modes += f"\x1b[?{encoding}h".encode()
                tab.emit(modes)
                prefix = f"{'alt' if alternate else 'main'}/mouse-{tracking}/encoding-{encoding}"
                wheels = b"".join(tab.mouse(code + mods) for mods in range(0, 32, 4) for code in (64, 65))
                case(prefix + "/wheel-modifiers", wheels)
                pointer = b"".join(tab.mouse(code + mods, release=release)
                                   for mods in (0, 8, 16, 24, 4, 12, 20, 28)
                                   for code, release in ((0, False), (0, True), (1, False), (1, True),
                                                         (2, False), (2, True), (32, False), (35, False)))
                case(prefix + "/click-drag-motion-modifiers", pointer)
    tab.resize(40, 360)
    wait_for(tab.status, lambda s: s and s["body"][2] == 358, "wide terminal")
    for encoding in (0, 1005, 1006, 1015):
        tab.emit(RESET + b"\x1b[?1000h" + (f"\x1b[?{encoding}h".encode() if encoding else b""))
        case(f"mouse/encoding-{encoding}/large-column", tab.mouse(0, x=260) + tab.mouse(0, x=260, release=True))
    tab.resize(40, 120)
    wait_for(tab.status, lambda s: s and s["body"][2] == 118, "restore terminal width")
    # Alternate scroll is separate from alternate screen; application-cursor arrows too.
    for alternate in (False, True):
        for enabled in (False, True):
            for app in (False, True):
                tab.emit(RESET + (b"\x1b[?1049h" if alternate else b"")
                         + (b"\x1b[?1007h" if enabled else b"\x1b[?1007l")
                         + (b"\x1b[?1h" if app else b""))
                case(f"alternate-scroll/screen-{alternate}/enabled-{enabled}/app-{app}",
                     tab.mouse(64) + tab.mouse(65))
    for app in (False, True):
        tab.emit(RESET + (b"\x1b[?1h" if app else b""))
        keys = [("up", "A"), ("down", "B"), ("right", "C"), ("left", "D"), ("home-key", "H"), ("end", "F")]
        for modifier in range(1, 9):
            for name, letter in keys:
                raw = f"\x1b[1;{modifier}{letter}".encode() if modifier > 1 else f"\x1b[{letter}".encode()
                case(f"keys/app-{app}/{name}/modifier-{modifier}", raw)
            for name, number in (("insert", 2), ("delete", 3), ("page-up", 5), ("page-down", 6),
                                 ("f5", 15), ("f6", 17), ("f7", 18), ("f8", 19), ("f9", 20),
                                 ("f10", 21), ("f11", 23), ("f12", 24)):
                raw = f"\x1b[{number};{modifier}~".encode() if modifier > 1 else f"\x1b[{number}~".encode()
                case(f"keys/app-{app}/{name}/modifier-{modifier}", raw)
            for number, letter in enumerate("PQRS", 1):
                raw = f"\x1b[1;{modifier}{letter}".encode() if modifier > 1 else f"\x1bO{letter}".encode()
                case(f"keys/app-{app}/f{number}/modifier-{modifier}", raw)
    tab.emit(RESET)
    for name, raw in (("text", "copper é".encode()), ("ctrl-a", b"\x01"), ("ctrl-alt-a", b"\x1b\x01"),
                      ("ctrl-shift-a", b"\x1b[97;6u"), ("ctrl-alt-shift-a", b"\x1b[97;8u"),
                      ("alt-a", b"\x1ba"), ("shift-A", b"A"), ("enter", b"\r"),
                      ("alt-enter", b"\x1b\r"), ("shift-enter", b"\x1b[13;2u"),
                      ("ctrl-enter", b"\x1b[13;5u"), ("tab", b"\t"), ("shift-tab", b"\x1b[Z"),
                      ("backspace", b"\x7f"), ("ctrl-backspace", b"\x08"), ("alt-backspace", b"\x1b\x7f"),
                      ("escape-alt-together", b"\x1ba")):
        case("characters/" + name, raw)
    # A lone Escape, read in its own poll, must arrive before the next character.
    start = len(tab.input())
    began = time.monotonic()
    tab.send(ESC)
    received = wait_for(tab.input, lambda raw: len(raw) > start, "lone Escape")
    assert received[start:] == ESC
    assert time.monotonic() - began < 1
    case("characters/escape-then-a", b"a", b"a")
    rows.append({"case": "characters/lone-escape", "sent": "1b", "received": received[start:].hex()})
    for keypad in (False, True):
        tab.emit(RESET + (b"\x1b=" if keypad else b"\x1b>"))
        # Numeric and enhanced keypad keys are both decoded; capture whether application
        # keypad mode preserves their distinct encoding.
        case(f"keypad/application-{keypad}/numeric", b"0123456789.+-*/\r")
        case(f"keypad/application-{keypad}/enhanced", b"".join(f"\x1b[{code}u".encode() for code in range(57399, 57417)))
    for enabled in (False, True):
        tab.emit(RESET + (b"\x1b[?2004h" if enabled else b""))
        case(f"paste/bracketed-{enabled}", b"\x1b[200~copper\nline\r\n\xce\xbb\x1b[201~")
        tab.emit(RESET + (b"\x1b[?1004h" if enabled else b""))
        case(f"focus/enabled-{enabled}", b"\x1b[I\x1b[O")
    for name, mode in (("kitty", b"\x1b[>31u"), ("modifyOtherKeys", b"\x1b[>4;2m")):
        tab.emit(RESET + mode)
        case(f"keyboard-protocol/{name}/shift-enter", b"\x1b[13;2u")
        case(f"keyboard-protocol/{name}/ctrl-alt-shift-a", b"\x1b[97;8u")
    # terminal-browser also asks for pixel mouse coordinates and keyboard event kinds.
    tab.emit(RESET + b"\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[>27u")
    case("terminal-browser/pixel-mouse-click", tab.mouse(0) + tab.mouse(0, release=True))
    for name, kind in (("press", 1), ("repeat", 2), ("release", 3)):
        case("terminal-browser/key-" + name, f"\x1b[97;1:{kind}u".encode())
    # The query's reply comes from the real PTY daemon, not the outer handshake.
    for name, raw in (("DA1", b"\x1b[c"), ("DA2", b"\x1b[>c"), ("XTVERSION", b"\x1b[>0q"),
                      ("DSR", b"\x1b[5n"), ("cursor", b"\x1b[6n"), ("window-pixels", b"\x1b[14t"),
                      ("cell-pixels", b"\x1b[16t"), ("window-cells", b"\x1b[18t"),
                      ("screen-cells", b"\x1b[19t"), ("OSC10", b"\x1b]10;?\x07"),
                      ("OSC11", b"\x1b]11;?\x07"), ("kitty-keyboard", b"\x1b[?u"),
                      ("OSC52-read", b"\x1b]52;c;?\x07"),
                      ("kitty-graphics", b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"),
                      ("XTSMGRAPHICS", b"\x1b[?1;1;0S")):
        tab.emit(RESET)
        received = tab.query(raw)
        rows.append({"case": "queries/" + name, "sent": raw.hex(), "received": received.hex()})
    for mode in (1, 66, 1000, 1002, 1003, 1004, 1005, 1006, 1007, 1015, 1016, 1049, 2004, 2026, 2031, 2048, 5522):
        for enabled in (False, True):
            tab.emit(RESET + f"\x1b[?{mode}{'h' if enabled else 'l'}".encode())
            raw = f"\x1b[?{mode}$p".encode()
            received = tab.query(raw)
            rows.append({"case": f"queries/DECRQM-{mode}/enabled-{enabled}", "sent": raw.hex(),
                         "received": received.hex()})
    for name, setting, query in (
        ("kitty-keyboard-enabled", b"\x1b[>31u", b"\x1b[?u"),
        ("modifyOtherKeys-enabled", b"\x1b[>4;2m", b"\x1b[?4m"),
    ):
        tab.emit(RESET + setting)
        received = tab.query(query)
        rows.append({"case": "queries/" + name, "sent": query.hex(), "received": received.hex()})
    # Observe protocol requests and rendering without invoking a real clipboard or browser.
    tab.emit(RESET + b"\x1b]52;c;Y29wcGVy\x07\x07\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\\x1b[6 q")
    status = wait_for(tab.status, lambda s: s and s["copied"] == "copper" and s["bell"], "clipboard and bell")
    assert "link" in status["text"]
    assert "SteadyBar" in status["cursor"], status["cursor"]
    rows.append({"case": "render/requests", "copied": status["copied"], "bell": status["bell"],
                 "hyperlink_text": "link" in status["text"], "cursor_bar": "SteadyBar" in status["cursor"]})
    for style, expected_shape in enumerate(("DefaultUserShape", "BlinkingBlock", "SteadyBlock",
                                             "BlinkingUnderScore", "SteadyUnderScore", "BlinkingBar", "SteadyBar")):
        tab.emit(RESET + f"\x1b[{style} q".encode())
        # Draw can precede the title sample in a frame; observe the next matching repaint.
        status = wait_for(tab.status, lambda s: s and expected_shape in s["cursor"], "cursor shape")
        cursor = status["cursor"]
        shape = cursor.split("style: ")[-1].split(" ")[0] if "style: " in cursor else cursor
        rows.append({"case": f"render/cursor-style-{style}", "shape": shape})
    tab.emit(RESET + b"\x1b[?25l")
    wait_for(tab.status, lambda s: s and s["cursor"] == "None", "hidden cursor")
    rows.append({"case": "render/cursor-hidden", "shape": "None"})
    tab.emit(b"\x1b[?25h")
    for name, image_bytes in (
        ("kitty", b"\x1b_Gi=32,s=1,v=1,a=T,t=d,f=24;AAAA\x1b\\"),
        ("sixel", b"\x1bPq#0;2;0;0;0~\x1b\\"),
    ):
        start = len(tab.output)
        tab.emit(RESET + image_bytes)
        time.sleep(0.03)
        outer = bytes(tab.output[start:])
        rows.append({"case": "images/" + name, "outer_graphics_bytes": image_bytes in outer})
    tab.emit(RESET + b"\x1b[?2026hSYNC\x1b[?2026l")
    wait_for(tab.status, lambda s: s and "SYNC" in s["text"], "synchronized output")
    rows.append({"case": "render/synchronized-output", "text": True})
    tab.emit(RESET)
    old = tab.status()["body"]
    tab.resize(44, 132)
    status = wait_for(tab.status, lambda s: s and s["body"][2:] != old[2:], "resize redraw")
    geometry = wait_for(lambda: json_lines(tab.root / "program.jsonl"),
                        lambda rows: any(row["kind"] == "resize" and [row["cols"], row["rows"]] == status["body"][2:] for row in rows),
                        "child SIGWINCH")
    rows.append({"case": "resize", "pane": status["body"][2:], "child_sigwinch": True})
    # Today's focused-terminal ownership: detach is unconditional. Other advertised space
    # chords go to the child after glass_key's terminal guard. Capture this distinction.
    for name, raw in (("ctrl-k", b"\x0b"), ("ctrl-q", b"\x11"), ("ctrl-s", b"\x13"),
                      ("ctrl-t", b"\x14"), ("ctrl-v", b"\x16"), ("ctrl-w", b"\x17"), ("ctrl-x", b"\x18"),
                      ("ctrl-h", b"\x08"), ("ctrl-o", b"\x0f"), ("ctrl-f", b"\x06"),
                      ("ctrl-c-shell", b"\x03"), ("ctrl-d-shell", b"\x04")):
        case("ownership/" + name, raw)
    start = len(tab.input())
    tab.send(b"\x1c")
    wait_for(tab.status, lambda s: s and s["detached"], "Ctrl-backslash detach")
    assert tab.input()[start:] == b"", "detach leaked into child"
    rows.append({"case": "ownership/ctrl-backslash", "sent": "1c", "received": "", "detached": True})
    assert tab.relay_error is None, tab.relay_error
    return rows


def agent_guards(worker):
    with tempfile.TemporaryDirectory(prefix="stui-guard-") as directory:
        tab = Tab(worker, [sys.executable, str(HERE / "copper_probe.py"), directory], directory)
        tab.env["STUI_PROBE_AGENT"] = "1"
        try:
            tab.start()
            tab.emit(RESET)
            rows = []
            for name, raw in (("ctrl-c", b"\x03"), ("ctrl-d", b"\x04")):
                for press in (1, 2):
                    received = tab.exercise(raw)
                    assert received == (b"" if press == 1 else raw)
                    rows.append({"case": f"ownership/{name}-agent/press-{press}",
                                 "sent": raw.hex(), "received": received.hex()})
            return rows
        finally:
            tab.close()


def check_program(worker, name, binary):
    with tempfile.TemporaryDirectory(prefix="stui-app-") as directory:
        root = Path(directory)
        document = root / ("document.html" if name == "cha" else "document.txt")
        lines = [f"copper line {number:03}" for number in range(200)]
        document.write_text("<html><body>" + "".join(f"<p>{line}</p>" for line in lines) + "</body></html>"
                            if name == "cha" else "\n".join(lines) + "\n")
        command = [binary, str(document)]
        if name == "vim":
            command = [binary, "-Nu", "NONE", "-n", "-c", "set mouse=a", str(document)]
        tab = Tab(worker, command, directory)
        try:
            tab.start()
            wait_for(tab.status, lambda s: s and "copper line" in s["text"], name + " document", seconds=15)
            time.sleep(0.2)
            before = tab.status()
            start = len(tab.wire_input)
            tab.send(tab.mouse(65))
            time.sleep(0.2)
            wheel = bytes(tab.wire_input[start:])
            after = tab.status()
            # Check the program itself can scroll: no network fixture or user input involved.
            start = len(tab.wire_input)
            tab.send(b"j" if name == "cha" else b"\x1b[B")
            time.sleep(0.2)
            keyboard = bytes(tab.wire_input[start:])
            assert keyboard, f"{name}: keyboard input did not reach the running program"
            assert after["text"] != tab.status()["text"], f"{name}: keyboard scroll did not change its screen"
            return {"program": name, "mode": before["mode"], "wheel_received": wheel.hex(),
                    "wheel_changed_screen": before["text"] != after["text"],
                    "key_received": keyboard.hex(), "key_changed_screen": after["text"] != tab.status()["text"]}
        finally:
            tab.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", required=True, type=Path)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--record", type=Path)
    parser.add_argument("--program", action="append", default=[], metavar="NAME=PATH")
    args = parser.parse_args()
    if not shutil.which("pty"):
        raise SystemExit("pty is required; use the repository's Nix development shell")
    with tempfile.TemporaryDirectory(prefix="stui-probe-") as directory:
        command = [sys.executable, str(HERE / "copper_probe.py"), directory]
        tab = Tab(args.worker.resolve(), command, directory)
        try:
            tab.start()
            rows = matrix(tab)
        except Exception:
            print("worker tail:", bytes(tab.output[-2000:]), file=sys.stderr)
            print("relay error:", tab.relay_error, file=sys.stderr)
            raise
        finally:
            tab.close()
    rows.extend(agent_guards(args.worker.resolve()))
    if args.check:
        expected = json.loads((HERE / "fixtures/terminal-tab-baseline.json").read_text())
        assert len(rows) == len(expected), f"row count {len(rows)} != {len(expected)}"
        for row, old in zip(rows, expected):
            assert row == old, f"{row['case']}:\nexpected {old}\nreceived {row}"
    if args.record:
        args.record.write_text(json.dumps(rows, indent=2) + "\n")
    print(f"terminal tab: {len(rows)} byte/render/resize cases passed")
    for program in args.program:
        name, binary = program.split("=", 1)
        print(json.dumps(check_program(args.worker.resolve(), name, binary), sort_keys=True))


if __name__ == "__main__":
    main()

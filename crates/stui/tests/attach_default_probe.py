#!/usr/bin/env python3
"""Open an agent in stui's real terminal tab and check what the person sees, through real PTYs.

stui (a production Ui, in the unit-test executable's worker) runs under one PTY, and the
agent's program under the packaged `pty` daemon. Between them is a transparent relay that this
harness can cut, refuse or let through, to drop the link the way a network does. Requires
Python 3 and `pty` (CI provides both). Only the graph is invented: no real agent, daemon graph,
or person's state; every case gets its own XDG_STATE_HOME.

Cases, each one stui:
  default        no saved choice: opening an agent attaches its terminal
  migration      a device's old saved choice (terminal_first true or false) still attaches
  opt-in         conversation_first saved: the agent opens on its conversation; the palette
                 entry "Conversation first" turns it off
  no-terminal    an agent without a terminal opens its conversation, with no error
  alert          an alert arriving while attached shows on top of the terminal; typed keys
                 still reach the program; a click shows it, a click answers it, and its "open
                 in Now" opens Home, which Ctrl+H closes onto the terminal
  reconnect      cutting the link shows "reconnecting" with its reason and the key to the
                 conversation; tries back off while the link is refused; the terminal comes
                 back by itself; Ctrl+\\ reaches the conversation while it is down
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
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
import select

WORKER = "ui::attach_default::attach_default_probe_worker"
AGENT = "agent/example/atlas/builder"
READY = "PROBE-READY"
PROGRAM = ["sh", "-c", f"echo {READY}; exec sleep 100000"]


def wait_for(read, predicate, label, seconds=10):
    deadline = time.monotonic() + seconds
    value = None
    while time.monotonic() < deadline:
        value = read()
        if value is not None and predicate(value):
            return value
        time.sleep(0.01)
    shown = dict(value or {})
    shown.pop("text", None)
    raise AssertionError(f"{label}: timed out; last={json.dumps(shown)[-900:]}")


def screen(status):
    return "\n".join(status["text"])


class Stui:
    """One stui worker, its program, and the relay between them."""

    def __init__(self, worker, directory, prefs=None):
        self.root = Path(directory)
        self.registry = self.root / "sessions"
        self.registry.mkdir()
        state = self.root / "state"
        if prefs is not None:
            (state / "st3" / "stui").mkdir(parents=True)
            (state / "st3" / "stui" / "prefs.json").write_text(json.dumps(prefs))
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self.env = {**os.environ, "TERM": "xterm-256color", "PTY_ROOT": str(self.registry),
                    "STUI_PROBE_ROOT": str(self.root), "STUI_PROBE_SOCKET": str(self.root / "relay.sock"),
                    "XDG_CONFIG_HOME": str(self.root / "config"),
                    "XDG_CACHE_HOME": str(self.root / "cache"),
                    "XDG_STATE_HOME": str(state), "HOME": str(self.root)}
        for name in ("ST_AGENT", "PTY_SESSION", "PTY_SESSION_DIR", "ST3_ENDPOINT", "ST3_PERSON"):
            self.env.pop(name, None)
        self.worker = worker
        self.process = None
        self.stopping = threading.Event()
        self.wire_input = bytearray()
        self.mode = "pass"  # pass: relay to the session; refuse: accept and close at once
        self.accepted = []  # when each connection came, and what was done with it
        self.current = None
        self.lock = threading.Lock()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.root / "relay.sock"))
        self.listener.listen(8)
        self.listener.settimeout(0.05)

    def pty_command(self, *args, check=True):
        result = subprocess.run([shutil.which("pty") or "pty", *args], env=self.env,
                                cwd=self.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
        if check and result.returncode:
            raise AssertionError(f"pty {args[0]}: {result.stderr.decode(errors='replace')}")
        return result

    def start(self):
        self.pty_command("run", "--force", "-d", "--id", "copper", "--", *PROGRAM)
        wait_for(lambda: (self.registry / "copper.sock").exists() or None, bool, "session socket")
        threading.Thread(target=self.accept, daemon=True).start()

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(
            [str(self.worker), "--exact", WORKER, "--ignored", "--nocapture"],
            stdin=self.slave, stdout=self.slave, stderr=self.slave, env=self.env,
            preexec_fn=controlling_terminal, cwd=self.root,
        )
        threading.Thread(target=self.drain, daemon=True).start()
        wait_for(self.status, lambda status: status.get("text"), "first frame")

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
            pending = (pending + data)[-4096:]
            # The outer terminal's capability handshake only.
            if b"\x1b[?u" in pending:
                os.write(self.master, b"\x1b[?1u")
                pending = pending.replace(b"\x1b[?u", b"")
            if b"\x1b[c" in pending:
                os.write(self.master, b"\x1b[?62;22c")
                pending = pending.replace(b"\x1b[c", b"")

    def accept(self):
        while not self.stopping.is_set():
            try:
                client, _ = self.listener.accept()
            except (socket.timeout, OSError):
                continue
            with self.lock:
                self.accepted.append((time.monotonic(), self.mode))
            if self.mode == "refuse":
                client.close()
                continue
            threading.Thread(target=self.relay, args=(client,), daemon=True).start()

    def relay(self, client):
        # A transparent transport tap: every packet goes unchanged to the real session.
        upstream = socket.socket(socket.AF_UNIX)
        upstream.connect(str(self.registry / "copper.sock"))
        with self.lock:
            self.current = (client, upstream)
        buffer = bytearray()
        try:
            while not self.stopping.is_set():
                ready = select.select([client, upstream], [], [], 0.05)[0]
                for source in ready:
                    data = source.recv(65536)
                    if not data:
                        return
                    (upstream if source is client else client).sendall(data)
                    if source is client:
                        buffer.extend(data)
                        while len(buffer) >= 5:
                            size = struct.unpack("!I", buffer[1:5])[0]
                            if len(buffer) < 5 + size:
                                break
                            if buffer[0] == 0:
                                self.wire_input.extend(buffer[5:5 + size])
                            del buffer[:5 + size]
        except OSError:
            return
        finally:
            for end in (client, upstream):
                try:
                    end.close()
                except OSError:
                    pass

    def cut(self):
        """Drop the link as a network does: both ends of the relay close, the program runs on."""
        with self.lock:
            current, self.current = self.current, None
        assert current, "nothing to cut"
        for end in current:
            try:
                end.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def status(self):
        try:
            return json.loads((self.root / "status.json").read_text())
        except (FileNotFoundError, json.JSONDecodeError):
            return None

    def action(self, value):
        staged = self.root / "ui-action.new"
        staged.write_text(value)
        staged.replace(self.root / "ui-action")

    def send(self, data):
        os.write(self.master, data)

    def click(self, rect):
        x, y = rect[0] + 2, rect[1] + 1
        self.send(f"\x1b[<0;{x};{y}M".encode())
        self.send(f"\x1b[<0;{x};{y}m".encode())

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
        os.close(self.master)
        os.close(self.slave)


def attached(stui):
    return wait_for(stui.status, lambda status: status["pane"] == f"terminal:{AGENT}"
                    and status["attached"] and READY in screen(status), "attached terminal")


def stays_on_conversation(stui, pane=f"agent:{AGENT}"):
    # Longer than the dwell before attaching, so an attach would have happened.
    time.sleep(1.0)
    status = stui.status()
    assert status["pane"] == pane, status["pane"]
    assert status["opened"] == 0, "a terminal was opened"
    return status


def case_default(stui):
    status = stui.status()
    assert not status["conversation_first"], "attaching is the default"
    stui.action("open")
    attached(stui)
    print("default: no saved choice, opening an agent attaches its terminal")


def case_migration(worker):
    for prefs in ({"terminal_first": True}, {"simple": True, "terminal_first": False}):
        with tempfile.TemporaryDirectory(prefix="stui-attach-") as directory:
            stui = Stui(worker, directory, prefs)
            try:
                stui.start()
                stui.action("open")
                attached(stui)
            finally:
                stui.close()
    print("migration: an old saved terminal_first, true or false, still attaches")


def case_opt_in(stui):
    status = wait_for(stui.status, lambda status: status["conversation_first"], "saved choice read")
    stui.action("open")
    stays_on_conversation(stui)
    # The palette's setting, by name.
    stui.send(b"\x0b")
    time.sleep(0.2)
    stui.send(b"conversation first")
    wait_for(stui.status, lambda status: "Conversation first" in screen(status), "palette entry shown")
    stui.send(b"\r")
    wait_for(stui.status, lambda status: not status["conversation_first"], "palette turned it off")
    print("opt-in: conversation first opens the conversation; its palette entry turns it off")


def case_no_terminal(stui):
    stui.action("open-unmanaged")
    status = wait_for(stui.status, lambda status: (status["pane"] or "").startswith("agent:")
                      and status["pane"] != f"agent:{AGENT}", "agent without a terminal opened")
    status = stays_on_conversation(stui, status["pane"])
    assert status["flash"] is None, status["flash"]
    print("no-terminal: an agent without one opens its conversation, and nothing is said")


def case_alert(stui):
    stui.action("open")
    attached(stui)
    stui.action("alert")
    status = wait_for(stui.status, lambda status: status["alerts"] and "1 alert" in screen(status),
                      "alert over the terminal")
    body = status["body"]
    band = [alert["rect"] for alert in status["alerts"]]
    assert all(rect[1] < body[1] for rect in band), f"the alert is above the terminal: {band} {body}"
    assert status["focused"] and READY in screen(status), "the terminal still has the keys"
    # Keys go to the terminal, never to the alert: Enter and y answer nothing.
    start = len(stui.wire_input)
    stui.send(b"y\r")
    wait_for(lambda: {"input": bytes(stui.wire_input[start:])},
             lambda seen: b"y\r" in seen["input"], "typed keys reached the program")
    assert stui.status()["answered"] == [], "a key answered the alert"
    # A click shows it; a click on an answer answers it, and nothing reaches the program.
    start = len(stui.wire_input)
    stui.click(band[0])
    status = wait_for(stui.status, lambda status: any(alert["tap"] == 'Answer("Yes")'
                                                      for alert in status["alerts"]), "alert shown")
    yes = next(alert["rect"] for alert in status["alerts"] if alert["tap"] == 'Answer("Yes")')
    assert yes[1] < status["body"][1], "the answer is above the terminal"
    stui.click(yes)
    wait_for(stui.status, lambda status: status["answered"] == ["ask/probe=Yes"], "answer sent")
    time.sleep(0.2)
    assert bytes(stui.wire_input[start:]) == b"", f"clicks reached the program: {bytes(stui.wire_input[start:])!r}"
    # Opening it works while attached: "open in Now" opens Home over the space, where Ctrl+H
    # closes it again onto the terminal. (While the terminal has the keys Ctrl+H is the
    # program's, a backspace to many; Home is this click, or Ctrl+K, away.)
    status = stui.status()
    now = next(alert["rect"] for alert in status["alerts"] if alert["tap"] == "Open")
    stui.click(now)
    wait_for(stui.status, lambda status: status["home"] and not status["focused"], "Home opened")
    stui.send(b"\x1b[104;5u")  # Ctrl+H as a kitty-keyboard terminal sends it
    wait_for(stui.status, lambda status: not status["home"] and status["focused"], "back on the terminal")
    print("alert: shown on top of the attached terminal; keys reach the program; clicks show, answer and open it in Now")


def case_reconnect(stui):
    stui.action("open")
    attached(stui)
    stui.cut()
    status = wait_for(stui.status, lambda status: status["lost"] and "reconnecting:" in screen(status),
                      "reconnecting shown")
    shown = screen(status)
    assert status["lost"] in shown, "the reason is said"
    assert "Ctrl+\\ conversation" in shown, "the key to the conversation is named"
    # Refused for a while: tries back off instead of looping.
    stui.mode = "refuse"
    refused_from = time.monotonic()
    time.sleep(4.0)
    with stui.lock:
        tries = [at for at, mode in stui.accepted if at >= refused_from]
    assert 1 <= len(tries) <= 3, f"{len(tries)} tries in 4s"
    gaps = [later - earlier for earlier, later in zip(tries, tries[1:])]
    assert all(gap >= 1.5 for gap in gaps), f"tries did not back off: {gaps}"
    status = stui.status()
    assert "reconnecting:" in screen(status) and status["pane"] == f"terminal:{AGENT}"
    # Let through: the terminal comes back by itself.
    stui.mode = "pass"
    wait_for(stui.status, lambda status: status["attached"] and not status["lost"]
             and READY in screen(status), "terminal back by itself", seconds=40)
    # Down again, the conversation is one keystroke away.
    stui.cut()
    wait_for(stui.status, lambda status: status["lost"], "reconnecting again")
    stui.send(b"\x1b[92;5u")  # Ctrl+\ as a kitty-keyboard terminal sends it
    wait_for(stui.status, lambda status: status["pane"] == f"agent:{AGENT}", "Ctrl+\\ to the conversation")
    print(f"reconnect: says why and the key; {len(tries)} tries in 4s refused, gaps {[round(gap, 1) for gap in gaps]}s; back by itself; Ctrl+\\ leaves")


CASES = {
    "default": (None, case_default),
    "opt-in": ({"conversation_first": True}, case_opt_in),
    "no-terminal": (None, case_no_terminal),
    "alert": (None, case_alert),
    "reconnect": (None, case_reconnect),
}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--worker", required=True)
    parser.add_argument("--case", action="append", choices=[*CASES, "migration"])
    args = parser.parse_args()
    if not shutil.which("pty"):
        raise SystemExit("pty is required; use the repository's Nix development shell")
    for name in args.case or [*CASES, "migration"]:
        if name == "migration":
            case_migration(args.worker)
            continue
        prefs, run = CASES[name]
        with tempfile.TemporaryDirectory(prefix="stui-attach-") as directory:
            stui = Stui(args.worker, directory, prefs)
            try:
                stui.start()
                run(stui)
            finally:
                stui.close()


if __name__ == "__main__":
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    main()

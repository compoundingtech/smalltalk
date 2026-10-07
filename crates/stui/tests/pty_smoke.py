#!/usr/bin/env python3
"""Exercise the built stui in disposable PTYs, with no network or real daemon.

Run all cases: cargo test -p stui --test pty_smoke --test typed_keys
Run one case: python3 crates/stui/tests/pty_smoke.py target/debug/stui --case normal
Cases: normal, signal, panic, delayed-getter, hangup, tmux-hangup, shifted-keys.
Release binaries: add --no-panic (the panic hook exists only with debug assertions).
Cargo requires tmux; standalone runs skip its case when tmux is unavailable.
The script reports checks and byte counts, never terminal contents. Each invocation
uses fresh XDG directories and removes ST_AGENT, ST3_ENDPOINT and graphics probes.
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import os
import pty
import re
import select
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time


FIRST_FRAME = b" working"  # Present even before st connects.
FRAME_END = b"\x1b[?2026l"  # stui's synchronized-update boundary.


def plain(output: bytes) -> bytes:
    return re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]", b"", output)


def isolated_env(directory: str) -> dict[str, str]:
    env = os.environ.copy()
    for name in ("ST_AGENT", "ST3_ENDPOINT", "STUI_TEST_PANIC_AFTER_ENTER",
                 "TERM_PROGRAM", "KITTY_WINDOW_ID"):
        env.pop(name, None)
    env.update(TERM="xterm-256color", ST3_PERSON="person/alex")
    for name in ("XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"):
        path = os.path.join(directory, name)
        os.mkdir(path, mode=0o700)
        env[name] = path
    return env


class Terminal:
    def __init__(self, binary: str, env: dict[str, str], keyboard: bool = False):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        self.before = termios.tcgetattr(slave)
        try:
            self.proc = subprocess.Popen([binary, "--local"], stdin=slave, stdout=slave,
                                         stderr=slave, env=env)
        except BaseException:
            os.close(self.master)
            raise
        finally:
            os.close(slave)
        self.captured = bytearray()
        self.keyboard = keyboard
        self.answered: dict[bytes, int] = {}
        self.eof = False

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.wait(timeout=3)
        if self.master is not None:
            os.close(self.master)

    def read(self, timeout: float) -> None:
        if self.eof:
            return
        ready, _, _ = select.select([self.master], [], [], timeout)
        if not ready:
            return
        try:
            chunk = os.read(self.master, 65536)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            chunk = b""
        if not chunk:
            self.eof = True
            return
        self.captured.extend(chunk)
        # Queries and responses can cross read boundaries. Answer each query once.
        for query, reply in ((b"\x1b[?u", b"\x1b[?0u" if self.keyboard else b""),
                             (b"\x1b[c", b"\x1b[?62;c")):
            count = self.captured.count(query)
            for _ in range(count - self.answered.get(query, 0)):
                if reply:
                    os.write(self.master, reply)
            self.answered[query] = count

    def wait_for(self, marker: bytes, timeout: float = 2, start: int = 0,
                 frame: bool = True) -> float:
        started = time.monotonic()
        deadline = started + timeout
        while True:
            output = bytes(self.captured[start:])
            # Only acknowledge a complete frame: its tail can otherwise look like the
            # response to the next key, or an Esc can join that key into an Alt chord.
            if frame:
                boundary = output.rfind(FRAME_END)
                output = output[:boundary] if boundary >= 0 else b""
            if marker in (output if marker.startswith(b"\x1b") else plain(output)):
                return time.monotonic() - started
            remaining = deadline - time.monotonic()
            assert remaining > 0 and not self.eof, (
                f"marker {marker!r} was not seen in {timeout:g} s "
                f"(exit {self.proc.poll()}, {len(self.captured)} bytes)"
            )
            self.read(remaining)

    def key(self, key: bytes, marker: bytes) -> None:
        start = len(self.captured)
        os.write(self.master, key)
        latency = self.wait_for(marker, timeout=1, start=start)
        assert latency < 0.5, f"{key!r} took {latency:.3f}s to redraw"

    def first_frame(self) -> float:
        elapsed = self.wait_for(FIRST_FRAME)
        assert elapsed < 1, f"first frame took {elapsed:.3f}s"
        assert b"\x1b[?1000h" in self.captured, "mouse capture was not enabled"
        return elapsed

    def keys(self) -> None:
        self.key(b"\x0b", b"open in")  # Ctrl+K: palette.
        self.key(b"\x1b", b"Checking what needs you")  # Esc: Home restored.
        self.key(b"\x13", b"Agents")  # Ctrl+S: show sidebar.
        self.key(b"\x13", b"Checking what needs you")  # Ctrl+S: hide sidebar.
        assert self.proc.poll() is None, "TUI exited while navigating"

    def exit(self, panic: bool = False) -> None:
        deadline = time.monotonic() + 3
        while not self.eof and time.monotonic() < deadline:
            self.read(deadline - time.monotonic())
        try:
            self.proc.wait(timeout=max(0.01, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            raise AssertionError("TUI did not exit") from None
        assert (self.proc.returncode == 0) == (not panic), (
            f"unexpected exit {self.proc.returncode}"
        )
        for marker in (b"\x1b[?1000l", b"\x1b[?1049l"):
            assert marker in self.captured, f"terminal mode {marker!r} was not restored"
        assert termios.tcgetattr(self.master) == self.before, "terminal stayed in raw mode"


def run_case(binary: str, ending: str, env: dict[str, str]) -> None:
    if ending == "panic":
        env = {**env, "STUI_TEST_PANIC_AFTER_ENTER": "1"}
    with Terminal(binary, env) as terminal:
        if ending == "panic":
            terminal.wait_for(b"\x1b[?1049h", frame=False)
        else:
            first_frame = terminal.first_frame()
            terminal.keys()
            if ending == "normal":
                os.write(terminal.master, b"\x11")  # Ctrl+Q.
            else:
                terminal.proc.send_signal(signal.SIGTERM)
        terminal.exit(panic=ending == "panic")
    if ending == "panic":
        print("panic: restoration OK")
    else:
        print(f"{ending}: first frame {first_frame:.3f}s, keys <0.5s, restoration OK")


def delayed_getter_case(binary: str, env: dict[str, str]) -> None:
    # A local socket deliberately never answers. No daemon or invented response shapes.
    endpoint = os.path.join(env["XDG_RUNTIME_DIR"], "st3.sock")
    accepted = threading.Event()
    stopping = threading.Event()
    connections: list[socket.socket] = []
    with socket.socket(socket.AF_UNIX) as server:
        server.bind(endpoint)
        server.listen(5)
        server.settimeout(0.1)

        def serve() -> None:
            while not stopping.is_set():
                try:
                    connection, _ = server.accept()
                except socket.timeout:
                    continue
                connections.append(connection)
                accepted.set()

        thread = threading.Thread(target=serve)
        thread.start()
        try:
            with Terminal(binary, env) as terminal:
                terminal.first_frame()
                assert accepted.wait(timeout=2), "TUI never attempted the getter"
                terminal.keys()
                os.write(terminal.master, b"\x11")
                terminal.exit()
        finally:
            stopping.set()
            thread.join(timeout=1)
            for connection in connections:
                connection.close()
    print("delayed getter: navigation and quit while the getter is outstanding")


def hangup_case(binary: str, env: dict[str, str]) -> None:
    with Terminal(binary, env) as terminal:
        terminal.first_frame()
        terminal.keys()  # The event reader has handled real input; no startup sleep.
        os.close(terminal.master)
        terminal.master = None
        try:
            terminal.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            raise AssertionError("hangup: TUI survived terminal close") from None
        assert terminal.proc.returncode == 0, f"hangup: exit {terminal.proc.returncode}"
    print("hangup: exited after PTY close")


def tmux_hangup_case(binary: str, env: dict[str, str]) -> None:
    tmux = shutil.which("tmux") or "/opt/homebrew/bin/tmux"
    assert os.path.exists(tmux), "tmux is required by the full PTY smoke (provided by CI)"
    base = [tmux, "-f", "/dev/null", "-L", f"stui-pty-smoke-{os.getpid()}"]
    # Control mode streams real pane bytes, so readiness needs no sleep or ps race.
    control = subprocess.Popen(
        base + ["-C", "new-session", "-s", "hup", "-x", "100", "-y", "24",
                f"exec {shlex.quote(binary)} --local"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env,
    )
    pid = None
    pidfd = None
    exited = False
    try:
        output = bytearray()
        pending = bytearray()
        deadline = time.monotonic() + 3
        while FIRST_FRAME not in plain(output) or FRAME_END not in output:
            remaining = deadline - time.monotonic()
            assert remaining > 0, "tmux hangup: no first frame"
            ready, _, _ = select.select([control.stdout], [], [], remaining)
            assert ready, "tmux hangup: no first frame"
            chunk = os.read(control.stdout.fileno(), 65536)
            assert chunk, "tmux hangup: control session closed before first frame"
            pending.extend(chunk)
            while b"\n" in pending:
                line, _, rest = pending.partition(b"\n")
                pending = bytearray(rest)
                if line.startswith(b"%output "):
                    pane = line.split(b" ", 2)[2]
                    output.extend(re.sub(rb"\\([0-7]{3})",
                                         lambda match: bytes([int(match[1], 8)]), pane))
        pid = int(subprocess.check_output(
            base + ["display-message", "-p", "-t", "hup:0.0", "#{pane_pid}"], env=env,
        ))
        if hasattr(os, "pidfd_open"):
            pidfd = os.pidfd_open(pid)
        subprocess.run(base + ["kill-session", "-t", "hup"], env=env,
                       check=True, capture_output=True, timeout=3)
        if pidfd is not None:
            ready, _, _ = select.select([pidfd], [], [], 3)
            assert ready, "tmux hangup: TUI survived session close"
        else:
            # Darwin lacks pidfds; await process disappearance instead of a startup delay.
            deadline = time.monotonic() + 3
            while True:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                assert time.monotonic() < deadline, "tmux hangup: TUI survived session close"
                select.select([], [], [], 0.02)
        exited = True
        print("tmux hangup: exited after session close")
    finally:
        subprocess.run(base + ["kill-server"], env=env, capture_output=True, timeout=3)
        if pid is not None and not exited:
            try:
                if pidfd is not None and hasattr(signal, "pidfd_send_signal"):
                    signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                else:
                    os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if pidfd is not None:
            os.close(pidfd)
        control.communicate(timeout=3)


def shifted_keys_case(binary: str, env: dict[str, str]) -> None:
    """Play a keyboard-protocol terminal; send shifted keys as real escape codes."""
    with Terminal(binary, env, keyboard=True) as terminal:
        terminal.first_frame()
        pushed = re.findall(rb"\x1b\[>(\d+)u", terminal.captured)
        assert pushed, "stui did not ask the terminal for the keyboard protocol"
        flags = int(pushed[-1])
        assert flags & 4, f"stui did not ask for alternate keys (flags {flags})"
        terminal.key(b"\x0b", b"open in")
        # Shift+i and Shift+; include the layout's character; Shift+j has no alternate.
        # The renderer may update only the new cell and the following input cursor.
        for sequence, typed in ((b"\x1b[105:73;2u", "I"),
                                (b"\x1b[59:58;2u", ":"), (b"\x1b[106;2u", "J")):
            terminal.key(sequence, (typed + "▏").encode())
        terminal.key(b"\x1b", b"Checking what needs you")
        os.write(terminal.master, b"\x11")
        terminal.exit()
    print("shifted keys: capitals and symbols typed from raw escape codes")


CASES = {
    "normal": lambda binary, env: run_case(binary, "normal", env),
    "signal": lambda binary, env: run_case(binary, "signal", env),
    "panic": lambda binary, env: run_case(binary, "panic", env),
    "delayed-getter": delayed_getter_case,
    "hangup": hangup_case,
    "tmux-hangup": tmux_hangup_case,
    "shifted-keys": shifted_keys_case,
}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/debug/stui")
    parser.add_argument("--case", choices=CASES)
    parser.add_argument("--no-panic", action="store_true")
    parser.add_argument("--only-shifted-keys", action="store_true")  # Existing cargo invocation.
    args = parser.parse_args()
    selected = [args.case] if args.case else list(CASES)
    if args.only_shifted_keys:
        selected = ["shifted-keys"]
    for case in selected:
        if case == "panic" and args.no_panic:
            print("panic: skipped (release binary has no debug panic hook)")
            continue
        if (case == "tmux-hangup" and args.case is None and not shutil.which("tmux")
                and not os.path.exists("/opt/homebrew/bin/tmux")):
            print("tmux hangup: skipped (tmux unavailable)")
            continue
        with tempfile.TemporaryDirectory(prefix="stui-smoke-") as directory:
            CASES[case](os.path.abspath(args.binary), isolated_env(directory))

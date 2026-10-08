#!/usr/bin/env python3
"""Exercise the built TUI (spaces) in a real PTY, with or without a local st daemon.

Run with ST3_PERSON=person/<you> python3 crates/stui/tests/pty_smoke.py.
The script reports only checks and byte counts; it never prints terminal contents.
"""

from __future__ import annotations

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
import sys
import tempfile
import termios
import threading
import time


FIRST_FRAME = b" working"  # The status bar's count is present even before st connects.


def plain(output: bytes) -> bytes:
    return re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]", b"", output)


def answer_queries(master: int, chunk: bytes) -> None:
    """Answer stui's device-attributes query as a terminal does, so its keyboard-protocol
    probe ends at once instead of waiting out its timeout."""
    if b"\x1b[c" in chunk:
        os.write(master, b"\x1b[?62;c")


def entrypoint_case(binary: str) -> None:
    # A seat or either redirected stream must retain bare-st help without terminal modes.
    for seat, terminal_in, terminal_out in ((True, True, True), (False, False, True),
                                            (False, True, False), (False, False, False)):
        master, slave = pty.openpty()
        env = os.environ.copy()
        env.pop("ST_AGENT", None)
        if seat:
            env["ST_AGENT"] = "agent/demo"
        try:
            result = subprocess.run([binary], stdin=slave if terminal_in else subprocess.DEVNULL,
                                    stdout=slave if terminal_out else subprocess.PIPE,
                                    stderr=subprocess.PIPE, env=env, timeout=5)
            output = result.stderr + (result.stdout or b"")
            while select.select([master], [], [], 0)[0]:
                output += os.read(master, 65536)
            assert result.returncode == 2, f"bare st help returned {result.returncode}"
            assert b"Usage: st" in output, "bare st did not show help"
            assert b"\x1b[?1049h" not in output, "bare st entered the alternate screen"
        finally:
            os.close(master)
            os.close(slave)
    print("bare st: seats and either redirected stream retain help, exit 2")


def tui_command(binary: str) -> list[str]:
    return [binary, "ui"] if "--ui" in sys.argv[2:] else [binary]


def run_case(binary: str, ending: str, endpoint: str | None = None) -> None:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    env = os.environ.copy()
    env["TERM"] = "xterm-256color"
    if endpoint:
        env["ST3_ENDPOINT"] = endpoint
    if ending == "panic":
        env["STUI_TEST_PANIC_AFTER_ENTER"] = "1"
    proc = subprocess.Popen(tui_command(binary), stdin=slave, stdout=slave, stderr=slave, env=env)
    os.close(slave)
    captured = bytearray()

    def collect(seconds: float) -> bytes:
        output = bytearray()
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                try:
                    chunk = os.read(master, 65536)
                    output.extend(chunk)
                    captured.extend(chunk)
                    answer_queries(master, chunk)
                except OSError:
                    break
        return bytes(output)

    def wait_for(marker: bytes, timeout: float) -> tuple[bytes, float]:
        started = time.monotonic()
        output = bytearray()
        while time.monotonic() - started < timeout:
            ready, _, _ = select.select([master], [], [], 0.02)
            if ready:
                try:
                    chunk = os.read(master, 65536)
                    output.extend(chunk)
                    captured.extend(chunk)
                    answer_queries(master, chunk)
                except OSError:
                    break
                if marker in (output if marker.startswith(b"\x1b") else plain(output)):
                    break
        return bytes(output), time.monotonic() - started

    marker = b"\x1b[?1049h" if ending == "panic" else FIRST_FRAME
    initial, first_frame = wait_for(marker, 2)
    # wait_for returns its elapsed time whether or not the marker came, so a stale marker would
    # read as a slow first frame (as it did when the top bar lost its label); say what is missing.
    assert marker in (initial if marker.startswith(b"\x1b") else plain(initial)), (
        f"first frame marker {marker!r} was not seen in 2 s"
    )
    assert first_frame < 1, f"first frame took {first_frame:.3f}s"
    if ending != "panic":
        assert b"\x1b[?1000h" in captured, "mouse capture was not enabled"
        # Ctrl+K opens the palette and Esc closes it; Ctrl+S shows the sidebar and hides it.
        for key in (b"\x0b", b"\x1b", b"\x13", b"\x13"):
            os.write(master, key)
            changed, latency = wait_for(b"", 1)
            assert changed, f"{key!r} did not redraw"
            assert latency < 0.5, f"{key!r} took {latency:.3f}s to redraw"
            # Apart, as typed: an Esc read together with the next key is Alt and that key.
            collect(0.2)
        collect(1)  # A live background snapshot may redraw after navigation.
        assert proc.poll() is None, f"{ending}: TUI exited before quit/signal ({proc.returncode})"
        if ending == "normal":
            os.write(master, b"\x11")  # Ctrl+Q
        else:
            proc.send_signal(signal.SIGTERM)
    deadline = time.monotonic() + 5
    while proc.poll() is None and time.monotonic() < deadline:
        collect(0.02)
    if proc.poll() is None:
        proc.kill()
        proc.wait()
        raise AssertionError(f"{ending}: TUI did not exit")
    collect(0.2)
    os.close(master)
    modes = sorted(set(re.findall(rb"\x1b\[\?[0-9;]*[hl]", captured)))
    assert ending == "panic" or b"\x1b[?1000l" in captured, f"{ending}: mouse capture was not released"
    assert b"\x1b[?1049l" in captured, (
        f"{ending}: alternate screen was not restored "
        f"(exit {proc.returncode}, {len(captured)} bytes, modes {modes}, tail {captured[-48:].hex()})"
    )
    assert (proc.returncode == 0) == (ending != "panic"), f"{ending}: unexpected exit {proc.returncode}"
    print(f"{ending}: restoration OK" if ending == "panic" else f"{ending}: first frame {first_frame:.3f}s, keys <0.5s, restoration OK")


def delayed_getter_case(binary: str) -> None:
    with tempfile.TemporaryDirectory(prefix="stui-delay-") as directory:
        endpoint = os.path.join(directory, "slow.sock")
        server = socket.socket(socket.AF_UNIX)
        server.bind(endpoint)
        server.listen(5)
        server.settimeout(0.2)
        stopping = threading.Event()

        def serve() -> None:
            while not stopping.is_set():
                try:
                    connection, _ = server.accept()
                except socket.timeout:
                    continue
                except OSError:
                    break
                # Keep the shared getter outstanding while the TUI handles keys.
                threading.Thread(target=lambda connection=connection: (time.sleep(5), connection.close()), daemon=True).start()

        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        try:
            run_case(binary, "normal", endpoint)
        finally:
            stopping.set()
            server.close()
            thread.join(timeout=1)


def hangup_case(binary: str) -> None:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    proc = subprocess.Popen(tui_command(binary), stdin=slave, stdout=slave, stderr=slave,
                            env={**os.environ, "TERM": "xterm-256color"})
    os.close(slave)
    deadline = time.monotonic() + 3
    output = bytearray()
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.1)
        if ready:
            chunk = os.read(master, 65536)
            output.extend(chunk)
            answer_queries(master, chunk)
            if FIRST_FRAME in plain(output):
                break
    else:
        proc.kill()
        raise AssertionError("hangup: no first frame")
    time.sleep(10)  # Exercise hangup after the live snapshot and event poll are running.
    os.close(master)
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        raise AssertionError("hangup: TUI survived terminal close")
    print("hangup: exited after PTY close")


def tmux_hangup_case(binary: str) -> None:
    tmux = shutil.which("tmux") or "/opt/homebrew/bin/tmux"
    if not os.path.exists(tmux):
        print("tmux hangup: skipped (tmux unavailable)")
        return
    socket_name = f"stui-pty-smoke-{os.getpid()}"
    target = "hup"
    base = [tmux, "-L", socket_name]
    subprocess.run(
        base + ["new-session", "-d", "-s", target,
                f"exec env ST3_PERSON=person/alex {shlex.join(tui_command(binary))}"],
        check=True, capture_output=True,
    )
    pid = int(subprocess.check_output(
        base + ["display-message", "-p", "-t", f"{target}:0.0", "#{pane_pid}"],
    ))
    try:
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            command = subprocess.check_output(["ps", "-p", str(pid), "-o", "comm="], text=True)
            if os.path.basename(binary) in command:
                break
            time.sleep(0.05)
        else:
            raise AssertionError("tmux hangup: TUI did not start")
        subprocess.run(base + ["kill-session", "-t", target], check=True, capture_output=True)
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                print("tmux hangup: exited after session close")
                return
            time.sleep(0.05)
        raise AssertionError(f"tmux hangup: TUI {pid} survived session close")
    finally:
        subprocess.run(base + ["kill-session", "-t", target], capture_output=True)
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def shifted_keys_case(binary: str) -> None:
    """A terminal that reports every key as an escape code sends Shift+i as `i` with Shift.
    Play one: answer the keyboard-protocol query, check what stui asks for, and type shifted keys
    into the palette's search box, as such a terminal would send them."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    env = os.environ.copy()
    env["TERM"] = "xterm-256color"
    proc = subprocess.Popen(tui_command(binary), stdin=slave, stdout=slave, stderr=slave, env=env)
    os.close(slave)
    captured = bytearray()

    def collect(seconds: float) -> None:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(master, 65536)
            except OSError:
                return
            captured.extend(chunk)
            if b"\x1b[?u" in chunk:
                os.write(master, b"\x1b[?0u")  # Supports the keyboard protocol.
            if b"\x1b[c" in chunk:
                os.write(master, b"\x1b[?62;c")

    try:
        collect(1.5)
        pushed = re.findall(rb"\x1b\[>(\d+)u", bytes(captured))
        assert pushed, "stui did not ask the terminal for the keyboard protocol"
        flags = int(pushed[-1])
        assert flags & 4, f"stui did not ask for alternate keys (flags {flags})"
        os.write(master, b"\x0b")  # Ctrl+K: the palette and its search box.
        collect(0.5)
        # Shift+i and Shift+; with the layout's character given, then Shift+j with none given.
        # The screen is redrawn in part, so each key is checked as the cell it newly wrote,
        # followed by the box's cursor.
        for sequence, typed in ((b"\x1b[105:73;2u", "I"), (b"\x1b[59:58;2u", ":"), (b"\x1b[106;2u", "J")):
            captured.clear()
            os.write(master, sequence)
            collect(0.5)
            text = plain(bytes(captured)).decode("utf-8", "replace")
            assert typed + "\u258f" in text, f"{sequence!r} did not type {typed!r} ({text[-120:]!r})"
        os.write(master, b"\x1b")
        collect(0.3)
        os.write(master, b"\x11")
        collect(0.5)
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait()
        os.close(master)
    print("shifted keys: capitals and symbols typed from raw escape codes")


if __name__ == "__main__":
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/debug/st3"
    if "--only-entrypoint" in sys.argv[2:]:
        entrypoint_case(binary)
        sys.exit(0)
    skip_panic = "--no-panic" in sys.argv[2:]
    if "--only-shifted-keys" in sys.argv[2:]:
        # The one case `cargo test` runs (../st3/tests/typed_keys.rs): keys as a terminal sends them.
        shifted_keys_case(binary)
        sys.exit(0)
    for case in ("normal", "signal", "panic"):
        if case == "panic" and skip_panic:
            print("panic: skipped (release binary has no debug panic hook)")
            continue
        run_case(binary, case)
    shifted_keys_case(binary)
    delayed_getter_case(binary)
    hangup_case(binary)
    tmux_hangup_case(binary)

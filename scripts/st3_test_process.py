#!/usr/bin/env python3
"""Own an isolated test's entire process tree, including double-forked PTY servers.

The guardian detaches from seat ancestry but retains pidfds for both its launcher and
the test runner. It is a Linux subreaper: orphaned descendants become its children,
so cleanup needs neither a live daemon API nor a scan of unrelated fleet processes.
"""
import argparse
import ctypes
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import time


def kill_group(pid):
    try:
        os.killpg(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def reap_descendants():
    """Kill before reaping, keeping each PID reserved while signalling its group.

    Killing an adopted parent can adopt more children (even ones in new sessions).
    Repeat until waitpid confirms we have no children at all.
    """
    children = Path(f"/proc/{os.getpid()}/task/{os.getpid()}/children")
    while True:
        for pid in map(int, children.read_text().split()):
            kill_group(pid)
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        try:
            while os.waitpid(-1, os.WNOHANG)[0]:
                pass
        except ChildProcessError:
            return
        time.sleep(0.01)


def guardian(command, watched, result):
    libc = ctypes.CDLL(None, use_errno=True)
    # PR_SET_CHILD_SUBREAPER, before launching any descendants.
    if libc.prctl(36, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "PR_SET_CHILD_SUBREAPER")
    interrupted = False

    def interrupt(signum, frame):
        nonlocal interrupted
        interrupted = True

    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, interrupt)
    task = None
    code = 1
    try:
        env = {**os.environ, "SMALLTALK_TEST_SUPERVISOR": str(os.getpid())}
        task = subprocess.Popen(command, env=env, start_new_session=True)
        with os.fdopen(os.pidfd_open(task.pid), "rb") as task_fd:
            poller = select.poll()
            for fd in [*watched, task_fd.fileno()]:
                poller.register(fd, select.POLLIN)
            # pidfds avoid PID reuse and the check/launch race. An already dead
            # owner is immediately readable, including after the double fork.
            while not interrupted and not poller.poll(100):
                pass
    finally:
        if task is not None:
            kill_group(task.pid)
            code = task.wait()
        reap_descendants()
        # Return status only AFTER every daemon, worker, driver and PTY is gone.
        os.write(result, str(code).encode())


def run_supervised(command, owner=None):
    # Open both before forking. The launcher waits for the guardian's result;
    # killing either the launcher or its Rust test runner cancels the whole tree.
    watched = [os.pidfd_open(owner or os.getppid()), os.pidfd_open(os.getpid())]
    reader, writer = os.pipe()
    intermediate = os.fork()
    if intermediate == 0:
        os.close(reader)
        os.setsid()
        if os.fork() != 0:
            os._exit(0)
        try:
            guardian(command, watched, writer)
        except BaseException as error:
            print(f"isolated test guardian: {error}", file=sys.stderr)
        finally:
            os._exit(0)
    os.close(writer)
    for fd in watched:
        os.close(fd)
    os.waitpid(intermediate, 0)
    try:
        status = os.read(reader, 32)
    finally:
        os.close(reader)
    code = int(status) if status else 1
    return code if code >= 0 else 128 - code


def supervised_main(main):
    """Also protect fixtures launched directly, outside Cargo/nextest."""
    if os.environ.get("SMALLTALK_TEST_SUPERVISOR"):
        return main()
    return run_supervised([sys.executable, *sys.argv])


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--owner", type=int)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("a command is required")
    raise SystemExit(run_supervised(command, args.owner))

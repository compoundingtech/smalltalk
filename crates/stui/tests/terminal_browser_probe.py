#!/usr/bin/env python3
"""Optional real terminal-browser probe; never opens a display or a terminal window.

Use a locally installed Linux v0.13.4 bundle with its Electron dependencies available.
This is not a CI dependency. The byte matrix covers its protocol requests independently.
"""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time

from terminal_tab_probe import Tab, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--library-path", default="", help="Optional library path for Electron only")
    parser.add_argument("--record", type=Path)
    args = parser.parse_args()
    app = args.app.resolve()
    electron = str(app / "electron/pixel")
    with tempfile.TemporaryDirectory(prefix="stui-browser-") as directory:
        root = Path(directory)
        document = root / "document.html"
        document.write_text("<html><body><h1>Copper browser probe</h1><p>Invented local content.</p></body></html>")
        tab = Tab(args.worker.resolve(), [], directory)
        for name in list(tab.env):
            if name.startswith(("CODEX_", "PIXEL_", "KITTY_")) or name in (
                "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "ELECTRON_RUN_AS_NODE", "LD_LIBRARY_PATH",
            ):
                tab.env.pop(name, None)
        tab.env.update({
            # Select the app's kitty renderer. No kitty process, screen or remote control is used.
            "TERM": "xterm-kitty", "XDG_RUNTIME_DIR": str(root / "runtime"),
            "XDG_DATA_HOME": str(root / "data"), "TERMINAL_BROWSER_DIST_ROOT": str(app),
            "TERMINAL_BROWSER_DISABLE_GPU": "1", "DISPLAY": "", "WAYLAND_DISPLAY": "",
            "DO_NOT_TRACK": "1", "TERMINAL_BROWSER_NO_TELEMETRY": "1",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=" + str(root / "absent-dbus.sock"),
        })
        (root / "runtime").mkdir(mode=0o700)
        browser_env = dict(tab.env)
        if args.library_path:
            browser_env["LD_LIBRARY_PATH"] = args.library_path
        with (root / "browser.log").open("wb") as log:
            # Start directly: the CLI's daemon setup can install a host AppArmor profile.
            # Headless Ozone and a private temporary profile require no host setup.
            daemon = subprocess.Popen([
                electron, str(app / "browser/dist/main.js"), "--ozone-platform=headless",
                "--screen-info={8192x8192}", "--no-sandbox", "--disable-gpu", "--use-angle=swiftshader",
            ], env=browser_env, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
            try:
                wait_for(lambda: list((root / "runtime").glob("*/daemon.sock")), bool,
                         "headless browser daemon", seconds=20)
                # Start the app after attachment so the transparent tap observes live output.
                command = [electron, str(app / "cli/dist/main.js"), "open", str(document)]
                child_env = {"ELECTRON_RUN_AS_NODE": "1"}
                if args.library_path:
                    child_env["LD_LIBRARY_PATH"] = args.library_path
                launcher = root / "launch.py"
                launcher.write_text(
                    "import os,time\nfrom pathlib import Path\n"
                    f"while not Path({str(root / 'status.json')!r}).exists():time.sleep(0.01)\n"
                    f"env=dict(os.environ)\nenv.update({child_env!r})\n"
                    'env["PIXEL_TTY"]="/proc/"+str(os.getpid())+"/root"+os.ttyname(0)\n'
                    f"os.execve({electron!r},{command!r},env)\n"
                )
                tab.command = ["python3", str(launcher)]
                tab.start()
                wait_for(lambda: bytes(tab.program_output), lambda data: data.count(b"\x1b_G") > 3,
                         "live browser graphics", seconds=20)
                before = tab.status()
                start = len(tab.wire_input)
                tab.send(tab.mouse(65))
                time.sleep(0.1)
                wheel = bytes(tab.wire_input[start:])
                start = len(tab.wire_input)
                tab.send(b"\x1b[97;8u")
                wait_for(lambda: bytes(tab.wire_input[start:]), bool, "browser modified key")
                raw = bytes(tab.program_output)
                receipt = {
                    "program": "terminal-browser", "version": "0.13.4", "headless": True,
                    "started": True, "mode": before["mode"],
                    "graphics_apc_count": raw.count(b"\x1b_G"),
                    "outer_graphics_apc_count": bytes(tab.output).count(b"\x1b_G"),
                    "wheel_received": wheel.hex(),
                    "ctrl_alt_shift_a_received": bytes(tab.wire_input[start:]).hex(),
                    "output_requests": sorted(set(match.decode("ascii") for match in
                                                  re.findall(rb"\x1b\[[0-9?;>$ ]*[a-zA-Z~]", raw))),
                }
            finally:
                tab.close()
                try:
                    os.killpg(daemon.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    daemon.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(daemon.pid, signal.SIGKILL)
                    daemon.wait()
        output = json.dumps(receipt, indent=2) + "\n"
        print(output, end="")
        if args.record:
            args.record.write_text(output)


if __name__ == "__main__":
    main()

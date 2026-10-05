#!/usr/bin/env python3
"""Model-free proof of the shipped channel against a real OMP ask picker.

Uses the native interrupted-ask fixture and actual F5 retry/terminal input, never
synthesized extension events. The channel peer records frames only; OMP owns tool
execution, UI, timeout, and results. All HOME/XDG/session state is disposable.
Run with --omp pointing to the raw installed OMP executable, not a fleet wrapper.
"""
import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

ANSI = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]")
TERMINAL_QUERY = re.compile(rb"\x1b\[(6n|c)")


class TerminalQueries:
    """Reply once per native query, regardless of PTY chunk boundaries."""
    def __init__(self):
        self.pending = b""

    def feed(self, chunk):
        data = self.pending + chunk
        replies = []
        consumed = 0
        for query in TERMINAL_QUERY.finditer(data):
            replies.append(b"\x1b[1;1R" if query[1] == b"6n" else b"\x1b[?1;2c")
            consumed = query.end()
        # At most three bytes can prefix either recognized query. Do not retain
        # an already-replied query, including the shorter three-byte DA1 query.
        self.pending = data[consumed:][-3:]
        return b"".join(replies)



def rows(path):
    if not path.exists():
        return []
    text = path.read_text()
    return [json.loads(line) for line in text[:text.rfind("\n") + 1].splitlines() if line]


def record_channel():
    directory = Path(os.environ["PROBE_CHANNEL_ROOT"])
    with (directory / "channel-pids").open("a") as output:
        output.write(f"{os.getpid()}\n")
    print(json.dumps({"type": "hello", "protocol": 1, "sessionContext": ""}), flush=True)
    with (directory / "frames.jsonl").open("a", buffering=1) as output:
        for line in sys.stdin:
            frame = json.loads(line)
            output.write(json.dumps({"channelPid": os.getpid(),
                                     "incarnation": os.environ["ST_OMP_CHANNEL_SESSION"],
                                     "runtimeId": os.environ["ST_OMP_CHANNEL_RUNTIME_ID"], **frame}) + "\n")
    with (directory / "channel-eof").open("a") as output:
        output.write(f"{os.getpid()}\n")


class Native:
    def __init__(self, root, args, timeout=0, transcript_rows=None, incarnation="one"):
        self.root = root
        root.mkdir()
        self.events = root / "events.jsonl"
        self.frames = root / "frames.jsonl"
        self.sessions = root / "sessions"
        self.sessions.mkdir()
        original = transcript_rows or rows(args.fixture)
        header = next(row for row in original if row["type"] == "session")
        header["cwd"] = str(root)
        self.session_id = header["id"]
        self.ask_id = next(row["data"]["toolCallId"] for row in original
                           if row.get("customType") == "tool_execution_start")
        self.transcript = self.sessions / f"2000-01-01T00-00-00-000Z_{self.session_id}.jsonl"
        self.transcript.write_text("".join(json.dumps(row) + "\n" for row in original))
        environment = {"PATH": os.environ.get("PATH", os.defpath), "TERM": "xterm-256color",
                       "PI_OFFLINE": "1", "NO_COLOR": "1", "PROBE_CHANNEL_ROOT": str(root),
                       # Admit the fixture's model without reading any real credentials.
                       # A post-result continuation can contact only this refused local port.
                       "ANTHROPIC_API_KEY": "native-ask-probe-inert",
                       "ANTHROPIC_BASE_URL": "http://127.0.0.1:1",
                       "ST_OMP_CHANNEL_BIN": str(root / "recorder"),
                       "ST_OMP_CHANNEL_CATALOG": str(root), "ST_OMP_CHANNEL_IDENTITY": "probe.worker",
                       "ST_OMP_CHANNEL_RUNTIME_ID": f"native-ask-{incarnation}",
                       "ST_OMP_CHANNEL_SESSION": incarnation, "ST_OMP_CHANNEL_SEQ": "1"}
        for name in ["HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME",
                     "XDG_DATA_HOME", "XDG_RUNTIME_DIR"]:
            directory = root / name.lower()
            directory.mkdir()
            environment[name] = str(directory)
        profile = Path(environment["HOME"]) / ".omp" / "agent"
        profile.mkdir(parents=True)
        (profile / "models.yml").write_text(
            "providers:\n  anthropic:\n    baseUrl: http://127.0.0.1:1\n"
            "    api: anthropic-messages\n    apiKey: native-ask-probe-inert\n"
            "    models:\n      - id: claude-opus-5-5\n        name: Native Ask Probe\n"
            "        contextWindow: 200000\n        maxTokens: 8192\n")
        recorder = root / "recorder"
        recorder.write_text(f"#!{sys.executable}\nimport runpy\nrunpy.run_path({str(Path(__file__).resolve())!r}, run_name='__main__')\n")
        recorder.chmod(0o755)
        observer = root / "observer.ts"
        observer.write_text('import fs from "node:fs";\nexport default function(pi) {\n'
                            f'const log = row => fs.appendFileSync({json.dumps(str(self.events))}, '
                            'JSON.stringify(row) + "\\n");\n'
                            'pi.on("session_start", (_event, ctx) => {\n'
                            'for (const method of ["select", "editor", "askDialog"]) {\n'
                            'const native = ctx.ui?.[method]; if (typeof native !== "function") continue;\n'
                            'ctx.ui[method] = function(...args) {\n'
                            'log({name: "native_ui_open", method, args});\n'
                            'return native.apply(this, args).finally(() => log({name: "native_ui_close", method}));\n'
                            '};\n}\n});\n'
                            'for (const name of ["session_start", "tool_call", "tool_execution_start", '
                            '"tool_execution_end", "tool_result", "agent_end", "session_shutdown", "before_provider_request"]) '
                            'pi.on(name, (event, ctx) => {\n'
                            'log({name, event, ctxKeys: Object.keys(ctx), uiKeys: Object.keys(ctx.ui ?? {})});\n'
                            # Stop after the real native ask result, before any model continuation.
                            'if (name === "tool_result" && event.toolName === "ask") ctx.abort();\n'
                            '});\n}\n')
        config = root / "config.yml"
        config.write_text(f"ask:\n  timeout: {timeout}\n  notify: false\n")
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self.child = subprocess.Popen([args.omp, "--session-dir", str(self.sessions), "--resume", self.session_id,
                                      "--no-extensions", "--no-skills", "--no-rules", "--no-lsp", "--no-title",
                                      "--model", "anthropic/claude-opus-5-5",
                                      "--config", str(config), "-e", str(args.extension), "-e", str(observer)],
                                     stdin=slave, stdout=slave, stderr=slave, cwd=root,
                                     env=environment, start_new_session=True)
        os.close(slave)
        self.output = bytearray()
        self.terminal_queries = TerminalQueries()
        self.closed = False

    def pump(self):
        if select.select([self.master], [], [], 0.05)[0]:
            try:
                chunk = os.read(self.master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                return
            self.output.extend(chunk)
            reply = self.terminal_queries.feed(chunk)
            if reply:
                os.write(self.master, reply)

    @property
    def screen(self):
        return ANSI.sub("", self.output.decode(errors="replace"))

    def until(self, predicate, description, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            result = predicate()
            if result:
                return result
            if self.child.poll() is not None:
                break
        raise AssertionError(f"{description}; native exit={self.child.poll()}\n{self.screen}\n"
                             f"events={json.dumps(rows(self.events))}\nframes={json.dumps(rows(self.frames))}")

    def state(self):
        return [frame for frame in rows(self.frames) if frame.get("type") == "state"]

    def start(self):
        self.until(lambda: "F5" in self.screen and "Retry" in self.screen, "historical ask has retry UI")
        self.until(lambda: self.state(), "initial channel state")
        assert all(frame.get("activeAsk") is None for frame in self.state()), "historical ask is not live"
        assert not any(row["name"] == "tool_execution_start" for row in rows(self.events))
        os.write(self.master, b"\x1b[15~")  # Real terminal F5; OMP retries the persisted tool call.
        self.until(lambda: any(row["name"] == "tool_execution_start" and
                               row["event"].get("toolCallId") == self.ask_id for row in rows(self.events)),
                   "native F5 emitted ask execution start")
        self.until(lambda: "Other (type your own)" in self.screen, "native ask picker is visible")
        self.until(lambda: any(frame.get("activeAsk") == self.ask_id for frame in self.state()),
                   "live native start publishes activeAsk")
        opened = self.until(lambda: next((row for row in rows(self.events)
                                         if row["name"] == "native_ui_open"), None),
                            "extension ctx.ui is the actual native picker context")
        assert opened["method"] in ("select", "askDialog"), opened
        return opened["method"]

    def reconnect(self, expected):
        previous = int((self.root / "channel-pids").read_text().splitlines()[-1])
        os.kill(previous, signal.SIGTERM)
        replacement = self.until(
            lambda: (int((self.root / "channel-pids").read_text().splitlines()[-1])
                     if int((self.root / "channel-pids").read_text().splitlines()[-1]) != previous else None),
            "channel reconnects within the same OMP incarnation")
        self.until(lambda: any(frame["channelPid"] == replacement and frame.get("activeAsk") == expected
                               for frame in self.state()), "reconnected channel reconfirms live ask state")
        assert all(frame.get("activeAsk") == expected for frame in self.state()
                   if frame["channelPid"] == replacement), self.state()

    def cleared(self):
        end = self.until(lambda: next((row for row in rows(self.events)
                                      if row["name"] == "tool_execution_end" and
                                      row["event"].get("toolCallId") == self.ask_id), None),
                         "native ask execution ended")
        self.until(lambda: self.state() and self.state()[-1].get("activeAsk") is None,
                   "native completion clears activeAsk")
        return end

    def close(self, signum=signal.SIGTERM):
        if self.closed:
            return
        if self.child.poll() is None:
            os.kill(self.child.pid, signum)
            deadline = time.monotonic() + 5
            while self.child.poll() is None and time.monotonic() < deadline:
                self.pump()
            if self.child.poll() is None:
                os.killpg(self.child.pid, signal.SIGKILL)
            self.child.wait()
        os.close(self.master)
        self.closed = True


def probe(args):
    args.omp = str(Path(args.omp).resolve(strict=True))
    version = subprocess.check_output([args.omp, "--version"], text=True).strip()
    print(f"runtime: {version}", flush=True)
    print(f"native executable: {args.omp}", flush=True)
    with tempfile.TemporaryDirectory(prefix="omp-native-active-ask-") as temporary:
        root = Path(temporary)
        active = []
        try:
            for action in ["answer", "cancel", "timeout", "exit", "crash"]:
                native = Native(root / action, args, timeout=3 if action == "timeout" else 0)
                active.append(native)
                picker = native.start()
                print(f"{action}: native ask-start -> activeAsk={native.ask_id}; picker visible", flush=True)
                if action == "answer":
                    print(f"native picker API: ctx.ui.{picker} (shared live UI promise)", flush=True)
                    native.reconnect(native.ask_id)
                    assert not any(row["name"] == "native_ui_close" for row in rows(native.events))
                    print("reconnect: same native picker promise still live -> same activeAsk", flush=True)
                if action in ["answer", "cancel"]:
                    os.write(native.master, b"\r" if action == "answer" else b"\x1b")
                if action not in ["exit", "crash"]:
                    ended = native.cleared()
                    result = ended["event"]["result"]
                    assert not any(row["name"] == "before_provider_request" for row in rows(native.events))
                    if action == "answer":
                        assert result["details"]["selectedOptions"] == ["Red"], result
                    elif action == "cancel":
                        assert ended["event"]["isError"] is True, ended
                        assert "cancel" in json.dumps(result).lower(), result
                    elif action == "timeout":
                        assert result["details"]["timedOut"] is True, result
                    print(f"{action}: native execution end -> no live activeAsk", flush=True)
                    if action == "answer":
                        # A second reconnect must not resurrect the completed ask.
                        native.reconnect(None)
                        print("reconnect after answer: no live native picker -> no activeAsk", flush=True)
                elif action == "exit":
                    native.close()
                    assert any(row["name"] == "session_shutdown" for row in rows(native.events))
                    assert native.state()[-1].get("activeAsk") is None, native.state()
                    assert native.state()[-1]["state"] == "ended", native.state()
                    print("exit: native shutdown -> no live activeAsk", flush=True)
                else:
                    native.close(signal.SIGKILL)
                    persisted = rows(native.transcript)
                    last_start = max(index for index, row in enumerate(persisted)
                                     if row.get("customType") == "tool_execution_start"
                                     and row.get("data", {}).get("toolCallId") == native.ask_id)
                    assert not any(row.get("message", {}).get("role") == "toolResult"
                                   and row["message"].get("toolCallId") == native.ask_id
                                   for row in persisted[last_start + 1:]), "crashed native ask remains pending"
                    restarted = Native(root / "restart", args, transcript_rows=persisted, incarnation="two")
                    active.append(restarted)
                    restarted.until(lambda: "Rebuilt bridge: which color?" in restarted.screen,
                                    "new incarnation renders the historical transcript")
                    restarted.until(lambda: restarted.state(), "new incarnation channel state")
                    assert all(frame.get("activeAsk") is None for frame in restarted.state())
                    assert all(frame["incarnation"] == "one" for frame in native.state())
                    assert all(frame["incarnation"] == "two" for frame in restarted.state())
                    assert not any(row["name"] == "tool_execution_start" for row in rows(restarted.events))
                    assert not any(row["name"] == "native_ui_open" for row in rows(restarted.events))
                    assert "Other (type your own)" not in restarted.screen
                    print("crash/restart: incarnation one -> two; historical pending transcript; no live picker; no activeAsk", flush=True)
                native.close()
            print("omp native active ask proof: ok", flush=True)
        finally:
            for native in active:
                native.close()


def main():
    if "PROBE_CHANNEL_ROOT" in os.environ:
        record_channel()
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--omp", default=os.environ.get("OMP_BIN"), required=not os.environ.get("OMP_BIN"))
    parser.add_argument("--fixture", type=Path, default=Path(__file__).with_name("native-interrupted-ask.jsonl"))
    parser.add_argument("--extension", type=Path, default=Path(__file__).resolve().parents[2] / "hooks/omp-channel.ts")
    args = parser.parse_args()
    try:
        probe(args)
        return 0
    except Exception as error:
        print(f"PROBE ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

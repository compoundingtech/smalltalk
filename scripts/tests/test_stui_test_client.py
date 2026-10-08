"""Controls for scripts/stui-test-client's sender-visibility oracle, run against a fake terminal
program: the oracle must not take the message box's echo for the conversation showing the message,
must find the marker through split escapes and split UTF-8, and must fail (nonzero) when the marker
never shows or is never echoed."""

import importlib.machinery
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "stui-test-client"
loader = importlib.machinery.SourceFileLoader("stui_test_client", str(SCRIPT))
spec = importlib.util.spec_from_loader("stui_test_client", loader)
client = importlib.util.module_from_spec(spec)
loader.exec_module(client)

FAKE = textwrap.dedent(
    '''\
    #!/usr/bin/env python3
    import os, sys, termios, time, tty
    mode = os.environ["FAKE_MODE"]
    fd = sys.stdin.fileno()
    tty.setraw(fd)
    out = lambda text: (os.write(1, text.encode()), time.sleep(0.01))
    out("\\x1b[?1049h\\x1b[2J\\x1b[1;1HConversation with an agent\\x1b[24;1H> ")
    typed = ""
    while True:
        data = os.read(fd, 1)
        if not data or data == b"\\x11":
            break
        ch = data.decode(errors="ignore")
        if ch == "\\r":
            if mode == "echo-only":
                continue                          # sent, but nothing ever shows it
            if mode == "after-clear":
                out("\\x1b[24;1H\\x1b[2K> ")   # the box clears at once
                time.sleep(0.15)                  # and the entry appears a moment later
            else:
                out("\\x1b[24;1H\\x1b[2K> ")
            if mode == "split":
                # the entry drawn through split escapes, colors inside, and a split UTF-8 character
                out("\\x1b[5;1")
                out("H\\x1b[3")
                out("2m")
                for letter in typed:
                    out(letter + "\\x1b[0m\\x1b[32m")
                out("\\xe2".encode("latin-1").decode("latin-1") if False else "")
                os.write(1, "\\u2713".encode()[:1]); time.sleep(0.02); os.write(1, "\\u2713".encode()[1:])
            else:
                out("\\x1b[5;1H" + typed)
            typed = ""
        elif ch.isprintable():
            if mode == "no-echo":
                continue
            typed += ch
            out(f"\\x1b[24;3H{typed}")
    '''
)


class Fake:
    def __init__(self, mode, test=None):
        self.dir = tempfile.TemporaryDirectory()
        if test is not None:
            test.addCleanup(self.dir.cleanup)
        self.binary = Path(self.dir.name) / "fake-stui"
        self.binary.write_text(FAKE)
        self.binary.chmod(0o755)
        self.events = Path(self.dir.name) / "events.jsonl"
        self.mode = mode

    def run(self, marker, timeout=2):
        env = dict(os.environ, FAKE_MODE=self.mode)
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--binary", str(self.binary), "--seconds", "0.1", "--start-wait", "0.3",
             "--rows", "30", "--columns", "80", "--events", str(self.events), "--say", marker, "--visible-timeout", str(timeout)],
            env=env, capture_output=True, text=True, timeout=60,
        )
        events = [json.loads(line) for line in self.events.read_text().splitlines()] if self.events.exists() else []
        return result, events


class Oracle(unittest.TestCase):
    def test_the_conversation_showing_the_marker_after_enter_is_visible_and_the_box_echo_is_not_enough(self):
        fake = Fake("normal", self)
        result, events = fake.run("fixture-visible-001")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        kinds = [event["type"] for event in events]
        self.assertEqual(kinds[:1], ["start"])
        self.assertLess(kinds.index("echo_seen"), kinds.index("send_key"))
        self.assertLess(kinds.index("send_key"), kinds.index("visible"))
        echo = next(event for event in events if event["type"] == "echo_seen")
        visible = next(event for event in events if event["type"] == "visible")
        self.assertFalse(set(echo["rows"]) & set(visible["rows"]), "visible rows differ from the echo rows")
        self.assertIn("epoch_ms", events[0])
        self.assertGreaterEqual(visible["latency_ms"], 0)

    def test_a_box_that_only_echoes_never_counts_as_visible_and_the_exit_is_nonzero(self):
        result, events = Fake("echo-only", self).run("fixture-echo-only-02", timeout=1)
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        kinds = [event["type"] for event in events]
        self.assertIn("echo_seen", kinds)
        self.assertIn("visible_timeout", kinds)
        self.assertNotIn("visible", kinds)

    def test_no_echo_at_all_is_a_distinct_nonzero_failure(self):
        result, events = Fake("no-echo", self).run("fixture-no-echo-0003", timeout=1)
        self.assertEqual(result.returncode, 4, result.stdout + result.stderr)
        self.assertIn("echo_timeout", [event["type"] for event in events])

    def test_a_marker_drawn_through_split_escapes_colors_and_utf8_is_found(self):
        result, events = Fake("split", self).run("fixture-split-0004")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("visible", [event["type"] for event in events])

    def test_an_entry_that_appears_a_moment_after_the_box_clears_is_timed_from_the_enter(self):
        result, events = Fake("after-clear", self).run("fixture-late-000005")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        visible = next(event for event in events if event["type"] == "visible")
        self.assertGreaterEqual(visible["latency_ms"], 100)

    def test_prose_is_refused_as_a_marker_and_no_events_hold_it(self):
        fake = Fake("normal", self)
        env = dict(os.environ, FAKE_MODE="normal")
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--binary", str(fake.binary), "--say", "hello there friend"],
            env=env, capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 2)
        self.assertFalse(fake.events.exists())

    def test_events_never_hold_the_keys_or_the_screen(self):
        fake = Fake("normal", self)
        _, events = fake.run("fixture-privacy-006")
        text = json.dumps(events)
        self.assertNotIn("Conversation with an agent", text)


class ScreenControls(unittest.TestCase):
    def test_cursor_moves_erases_and_wide_characters(self):
        screen = client.Screen(5, 20)
        screen.feed(b"\x1b[2;3Hhello\x1b[2;1H\x1b[K\x1b[3;1Hab\x1b[1;1H\x1b[0m")
        self.assertEqual(screen.lines()[1].strip(), "")
        self.assertEqual(screen.lines()[2][:2], "ab")
        screen.feed("\x1b[4;1H中x".encode())
        self.assertTrue(screen.lines()[3].startswith("中x"))

    def test_a_marker_split_across_reads_is_assembled(self):
        screen = client.Screen(3, 30)
        for piece in (b"\x1b[2", b";1Hfixt", b"ure-", b"abc12345"):
            screen.feed(piece)
        self.assertEqual(screen.rows_with("fixture-abc12345"), [1])


if __name__ == "__main__":
    unittest.main()

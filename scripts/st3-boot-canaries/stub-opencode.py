#!/usr/bin/env python3
"""A token-free `opencode`: the local HTTP/SSE server st's OpenCode driver talks to.

st runs `opencode --version`, then `opencode --port PORT --hostname 127.0.0.1` with
OPENCODE_SERVER_PASSWORD set. It checks GET /doc for the API surface it depends on, follows GET
/event (server-sent events), seeds from /session/status, /permission and /question, and delivers
each message with POST /session/ID/prompt_async. The driver treats a message as consumed when the
server reports an assistant message whose parentID is that messageID, so the stand-in answers every
prompt the way a model would: it acts on the message, then reports the assistant turn.
"""
import json
import os
import queue
import signal
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import stubmodel

SESSION = "ses_stub"
# The driver refuses a server whose OpenAPI document lacks any arm it uses.
DOC = {"openapi": "3.1.0", "info": {"title": "opencode stub"},
       "paths": {"/session/{id}/prompt_async": {"post": {"requestBody": {"messageID": "string"}}},
                 "/permission": {}, "/question": {}},
       "events": ["session.status", "session.idle", "session.error", "permission.asked",
                  "permission.replied", "question.asked", "question.replied", "question.rejected"]}
PROVIDERS = {"providers": [{"id": "opencode", "name": "OpenCode Zen", "source": "custom", "env": [],
                            "options": {}, "models": {"stub-model": {
                                "id": "stub-model", "providerID": "opencode", "name": "Stub",
                                "limit": {"context": 190000, "output": 64000}, "status": "active",
                                "cost": {"input": 0, "output": 0, "cache": {"read": 0, "write": 0}},
                                "options": {}, "headers": {}, "variants": {}}}}],
             "default": {"opencode": "stub-model"}}

messages = {}
streams = []
lock = threading.Lock()


def publish(event):
    with lock:
        for stream in list(streams):
            stream.put(event)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body=None):
        data = json.dumps({} if body is None else body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/doc":
            self.reply(200, DOC)
        elif path == "/event":
            self.events()
        elif path == "/session/status":
            self.reply(200, {})
        elif path in ("/permission", "/question"):
            self.reply(200, [])
        elif path == "/session":
            self.reply(200, [{"id": SESSION}])
        elif path == "/config/providers":
            self.reply(200, PROVIDERS)
        elif "/message/" in path:
            self.reply(200 if path.rsplit("/", 1)[1] in messages else 404)
        else:
            self.reply(404)

    def do_POST(self):
        path = self.path.split("?")[0]
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        if path == "/session":
            self.reply(200, {"id": SESSION})
        elif path == "/tui/select-session":
            self.reply(200, True)
        elif path.endswith("/prompt_async"):
            request = json.loads(body or b"{}")
            identifier = request.get("messageID")
            if not identifier:
                return self.reply(400)
            first = identifier not in messages
            text = "".join(part.get("text", "") for part in request.get("parts", []))
            messages[identifier] = text
            self.reply(204)
            if first:
                threading.Thread(target=answer, args=(identifier, text), daemon=True).start()
        else:
            self.reply(404)

    def events(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        feed = queue.Queue()
        with lock:
            streams.append(feed)
        try:
            self.wfile.write(b'data: {"type":"server.connected","properties":{}}\n\n')
            self.wfile.flush()
            while True:
                try:
                    event = feed.get(timeout=10)
                except queue.Empty:
                    event = {"type": "server.heartbeat", "properties": {}}
                self.wfile.write(b"data: " + json.dumps(event).encode() + b"\n\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            with lock:
                if feed in streams:
                    streams.remove(feed)


def answer(identifier, text):
    stubmodel.receipt("turn", text=text)
    publish({"type": "session.status", "properties": {"sessionID": SESSION, "status": {"type": "busy"}}})
    stubmodel.act(text)
    publish({"type": "message.updated", "properties": {"info": {
        "id": f"assistant-{identifier}", "role": "assistant", "parentID": identifier,
        "sessionID": SESSION}}})
    publish({"type": "session.idle", "properties": {"sessionID": SESSION}})
    publish({"type": "session.status", "properties": {"sessionID": SESSION, "status": {"type": "idle"}}})


def option(argv, name):
    for index, argument in enumerate(argv):
        if argument == name and index + 1 < len(argv):
            return argv[index + 1]
        if argument.startswith(name + "="):
            return argument.split("=", 1)[1]
    return None


def main(argv):
    if "--version" in argv:
        print("1.18.25")
        return 0
    stubmodel.receipt("started", argv=argv)
    port = int(option(argv, "--port") or 0)
    server = ThreadingHTTPServer((option(argv, "--hostname") or "127.0.0.1", port), Handler)
    server.daemon_threads = True

    def ended(number, _frame):
        stubmodel.receipt("signal", signal=number)
        os._exit(0)
    for number in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
        signal.signal(number, ended)
    stubmodel.receipt("ready", port=server.server_address[1])
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

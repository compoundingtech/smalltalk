#!/usr/bin/env python3
"""Admission probe: a channel event and a tool that records model receipt."""
import json
import os
from pathlib import Path
import sys
import threading
import time

case = os.environ["SPIKE_CASE"]
root = Path("/home/ada/results")
lock = threading.Lock()


def send(value):
    with lock:
        print(json.dumps(value), flush=True)


def event():
    time.sleep(3)
    send({"jsonrpc": "2.0", "method": "notifications/claude/channel",
          "params": {"content": "Call record_ack with text CHANNEL_SPIKE_ACK. Do nothing else.",
                     "meta": {"case": case}}})
    (root / (case + "-sent")).write_text("notification sent\n")


for line in sys.stdin:
    request = json.loads(line)
    with (root / (case + "-mcp.jsonl")).open("a") as out:
        out.write(json.dumps(request) + "\n")
    method = request.get("method")
    if method == "notifications/initialized":
        threading.Thread(target=event, daemon=True).start()
        continue
    if "id" not in request:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2025-03-26",
                  "capabilities": {"tools": {}, "experimental": {"claude/channel": {}}},
                  "serverInfo": {"name": "st-spike", "version": "1"},
                  "instructions": "Use record_ack to acknowledge the test channel event."}
    elif method == "tools/list":
        result = {"tools": [{"name": "record_ack", "description": "Acknowledge the probe event.",
                            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}},
                                            "required": ["text"]}}]}
    elif method == "tools/call":
        text = request["params"]["arguments"]["text"]
        (root / (case + "-ack")).write_text(text + "\n")
        result = {"content": [{"type": "text", "text": "Recorded."}]}
    else:
        result = {}
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})

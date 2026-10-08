#!/usr/bin/env python3
"""Token-free plugin CLI and consent adapter around the existing native boot fixture.

State belongs to the disposable guest. This simulates a provider contract, never an
account login or a paid model. Consent text was observed in Claude Code 2.1.293.
"""
import json
import os
from pathlib import Path
import runpy
import select
import sys
import termios
import time
import tty

import stubmodel

argv=sys.argv[1:]
state_path=Path.home()/".claude/onboarding-fixture.json"
state=json.loads(state_path.read_text()) if state_path.exists() else {"marketplaces":[],"plugins":[]}

def save():
    state_path.parent.mkdir(parents=True,exist_ok=True)
    state_path.write_text(json.dumps(state))

if "--version" in argv:
    print("2.1.293 (Claude Code fixture)")
    sys.exit(0)
if argv[:1]==["plugin"]:
    if argv[1:3]==["disable","--help"]:
        print("Usage: claude plugin disable [options] <plugin>")
    elif argv[1:4]==["marketplace","list","--json"]:
        print(json.dumps(state["marketplaces"]))
    elif argv[1:3]==["marketplace","add"]:
        path=Path(argv[3])
        name=json.loads((path/".claude-plugin/marketplace.json").read_text())["name"]
        state["marketplaces"]=[m for m in state["marketplaces"] if m["name"]!=name]+[{"name":name,"source":"directory","path":str(path)}]
        save()
    elif argv[1:3]==["marketplace","update"]:
        pass
    elif argv[1:3]==["list","--json"]:
        print(json.dumps(state["plugins"]))
    elif argv[1:2]==["install"]:
        state["plugins"]=sorted(set(state["plugins"]+[argv[2]])); save()
    elif argv[1:2]==["uninstall"]:
        state["plugins"]=[p for p in state["plugins"] if p!=argv[2]]; save()
    elif argv[1:2]==["disable"]:
        pass
    else:
        print("unsupported fixture plugin command",file=sys.stderr); sys.exit(2)
    sys.exit(0)

stubmodel.receipt("provider-argv",argv=argv)
if any(a.startswith("--dangerously-load-development-channels") for a in argv):
    print("WARNING: Loading development channels\n\n"
          "--dangerously-load-development-channels is for local channel development only.\n"
          "Please use --channels to run a list of approved channels.\n\n"
          "Channels: plugin:st-channel@st\n\n"
          "❯ 1. I am using this for local development\n  2. Exit\n\n"
          "Enter to confirm · Esc to cancel",flush=True)
    if not sys.stdin.isatty(): sys.exit("fixture consent requires a terminal")
    previous=termios.tcgetattr(0)
    accepted=False
    try:
        tty.setcbreak(0)
        deadline=time.monotonic()+25
        while time.monotonic()<deadline:
            if select.select([0],[],[],0.2)[0]:
                value=os.read(0,1)
                if value in (b"\r",b"\n"): accepted=True; break
                if value in (b"\x1b",b"2"): break
    finally:
        termios.tcsetattr(0,termios.TCSANOW,previous)
    stubmodel.receipt("development-consent",accepted=accepted)
    if not accepted: sys.exit(3)

# The shared Claude fixture supports an inline st3 MCP server. Resolve the installed
# packaged plugin to that same server while retaining the original route receipt.
if "--channels" in argv and "--mcp-config" not in argv and not any(a.startswith("--mcp-config=") for a in argv):
    if "st-channel@st" not in state["plugins"]: sys.exit("fixture plugin absent")
    server={"command":os.environ["ST3_BIN"],"args":["driver","claude-mcp"]}
    argv += ["--mcp-config",json.dumps({"mcpServers":{"st3":server}})]
sys.argv=[str(Path(__file__).with_name("stub-claude.py")),*argv]
runpy.run_path(sys.argv[0],run_name="__main__")

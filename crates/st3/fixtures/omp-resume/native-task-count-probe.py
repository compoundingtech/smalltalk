#!/usr/bin/env python3
"""Model-free real OMP parent task-count proof. Reuses #1496's native PTY helper.

Only the disposable terminal receives F5, Escape and /new. Native task execution
owns lifecycle events; this probe never synthesizes extension lifecycle events.
Every provider uses the helper's inert credentials and refused localhost endpoint.
"""
import argparse
import copy
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--omp", default=os.environ.get("OMP_BIN"), required=not os.environ.get("OMP_BIN"))
    parser.add_argument("--native-support", type=Path, default=Path(__file__).with_name("native-active-ask-probe.py"),
                        help="shared native PTY helper from #1496")
    parser.add_argument("--fixture", type=Path, default=Path(__file__).with_name("native-interrupted-ask.jsonl"))
    parser.add_argument("--extension", type=Path, default=Path(__file__).resolve().parents[2] / "hooks/omp-channel.ts")
    args = parser.parse_args()
    spec = importlib.util.spec_from_file_location("native_support", args.native_support.resolve(strict=True))
    support = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(support)
    transcript = copy.deepcopy(support.rows(args.fixture))
    task_input = {"context": "Disposable model-free task-count proof; do not contact any real provider.",
                  "tasks": [{"name": name, "agent": "task", "task": "Return one short result.",
                             "solutionSpace": "one fixed result"} for name in ["CountOne", "CountTwo"]]}
    for row in transcript:
        for content in row.get("message", {}).get("content", []):
            if isinstance(content, dict) and content.get("type") == "toolCall":
                content.update(name="task", arguments=task_input, intent="Launching count probe")
        if row.get("customType") == "tool_execution_start":
            row["data"].update(toolName="task", intent="Launching count probe")
        for call in row.get("data", {}).get("pendingToolCalls", []):
            call.update(toolName="task", args=task_input, intent="Launching count probe")
    with tempfile.TemporaryDirectory(prefix="omp-native-count-", dir="/dev/shm") as temporary:
        root = Path(temporary)
        lifecycle = root / "lifecycle.jsonl"
        extension = root / "count-extension.ts"
        extension.write_text('import fs from "node:fs";\nimport channel from '
                             + json.dumps(str(args.extension.resolve(strict=True)))
                             + ';\nexport default function(pi) { channel(pi); '
                             'pi.on("session_start", (_event, ctx) => { '
                             'if(ctx.agent?.kind === "sub") return; '
                             'pi.events.on("task:subagent:lifecycle", value => fs.appendFileSync('
                             + json.dumps(str(lifecycle))
                             + ', JSON.stringify({id:value.id,status:value.status})+"\\n")); }); }\n')
        native = support.Native(root / "native", SimpleNamespace(omp=str(Path(args.omp).resolve(strict=True)),
                                fixture=args.fixture, extension=extension), transcript_rows=transcript)
        try:
            native.until(lambda: "F5" in native.screen and "Retry" in native.screen, "native task retry UI")
            native.until(lambda: native.state(), "initial count frame")
            assert native.state()[-1]["runningSubagents"] == 0
            os.write(native.master, b"\x1b[15~")
            native.until(lambda: {row["id"] for row in support.rows(lifecycle) if row["status"] == "started"}
                         == {"CountOne", "CountTwo"}, "two real native task starts")
            native.until(lambda: native.state()[-1].get("runningSubagents") == 2, "parent count two")
            os.write(native.master, b"\x1b")
            native.until(lambda: native.state()[-1].get("state") == "idle", "parent idle with detached children")
            assert native.state()[-1]["runningSubagents"] == 2
            os.write(native.master, b"/new\r")
            native.until(lambda: len([row for row in support.rows(native.events) if row["name"] == "session_start"]) >= 2,
                         "native session replacement")
            native.until(lambda: native.state()[-1].get("runningSubagents") == 0, "successor count zero")
            assert {row["id"] for row in support.rows(lifecycle) if row["status"] == "aborted"} == {"CountOne", "CountTwo"}
            print(json.dumps({"result": "native parent task counts: passed", "lifecycle": support.rows(lifecycle),
                              "states": [{key: row.get(key) for key in ["state", "runningSubagents", "backgroundJobs"]}
                                         for row in native.state()]}, indent=2))
        finally:
            native.close()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(f"PROBE ERROR: {error}", file=sys.stderr)
        sys.exit(2)

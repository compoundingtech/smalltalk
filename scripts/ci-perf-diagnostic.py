#!/usr/bin/env python3
"""Hosted Performance diagnosis only. Never launched by source preparation."""
import hashlib
import json
import os
from pathlib import Path
import resource
import shutil
import signal
import subprocess
import sys
import time

SOURCES = {
    "705e2333ec6078589445438cf8d77fe3a1568c72",
    "58358162bbad1781c7e5111eacc57749d59a0915",
    "7fbd853ed3c5a2c81a29144ca6fffa36ab88a705",
    "1e20c15d11328b633b8ced9b1ccbb052db65ba32",
}
BASE = ["cargo", "test", "--release", "-p", "st3", "--features", "perf-load",
        "--test", "perf_load", "--locked"]
TEST = "daemon_load::the_daemon_keeps_its_budgets_under_a_busy_hosts_load"
OUT = Path(os.environ["RUNNER_TEMP"]) / "perf"
OUT.mkdir(exist_ok=True)
START = time.monotonic()
DEADLINE = START + min(1500, 1680 - (time.time() - float(os.environ["PERF_DIAGNOSTIC_WHOLE_START"])))
MAX_LOG = 64 * 1024 * 1024
MAX_CAPTURE = 2 * 1024 * 1024
MAX_HASH = 256 * 1024 * 1024
CHILD = None
TEST_EXIT = None
SPAWN_UNCERTAIN = False


def write(name, value):
    raw = json.dumps(value, sort_keys=True, indent=2).encode() + b"\n"
    if len(raw) > MAX_CAPTURE:
        raise RuntimeError("capture size limit")
    (OUT / name).write_bytes(raw)


def digest(path):
    p = Path(path)
    if not p.is_file() or p.stat().st_size > MAX_HASH:
        raise RuntimeError("invalid hash input or size limit")
    h = hashlib.sha256()
    with p.open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return {"sha256": h.hexdigest(), "bytes": p.stat().st_size}


def read(path, limit=65536):
    try:
        with open(path) as f:
            result = f.read(limit + 1)
        if len(result) > limit:
            raise RuntimeError("bounded read exceeded: " + path)
        return result
    except FileNotFoundError:
        return None


def snapshot(roster=False):
    stat = read("/proc/stat")
    ticks = [int(x) for x in stat.splitlines()[0].split()[1:]]
    result = {"wall_unix_ns": time.time_ns(), "monotonic": time.monotonic(),
              "cpu_ticks": ticks, "loadavg": read("/proc/loadavg"),
              "pressure": {k: read("/proc/pressure/" + k) for k in ["cpu", "io", "memory"]}}
    group = read("/proc/self/cgroup")
    if group and group.startswith("0::"):
        root = Path("/sys/fs/cgroup") / group.strip().split("::", 1)[1].lstrip("/")
        result["cgroup_stats"] = {k: read(str(root / k)) for k in
                                  ["cpu.stat", "cpu.max", "memory.current", "memory.max", "pids.current", "pids.max"]}
    if roster:
        pids = sorted(p for p in Path("/proc").iterdir() if p.name.isdecimal())
        if len(pids) > 256:
            raise RuntimeError("process population over capture ceiling")
        processes = []
        for p in pids:
            try:
                text = read(str(p / "stat"), 8192)
                if text is None:
                    continue
                parts = text.rsplit(") ", 1)[1].split()
                processes.append({"pid": int(p.name), "comm": read(str(p / "comm"), 256),
                                  "state": parts[0], "ppid": int(parts[1]),
                                  "session": int(parts[3]), "utime": int(parts[11]),
                                  "stime": int(parts[12]), "start_ticks": int(parts[19])})
            except (FileNotFoundError, ProcessLookupError):
                continue
        result["processes"] = processes
    return result


def cpu_delta(before, after):
    # guest/guest_nice are already included in user/nice; do not count twice.
    a, b = before["cpu_ticks"], after["cpu_ticks"]
    delta = [y - x for x, y in zip(a[:8], b[:8])]
    total = sum(delta)
    if total <= 0 or any(x < 0 for x in delta):
        raise RuntimeError("invalid CPU counter interval")
    return {"busy_fraction": (total - delta[3] - delta[4]) / total,
            "iowait_fraction": delta[4] / total,
            "steal_fraction": delta[7] / total}


def stop_child():
    global CHILD
    if CHILD is None or CHILD.poll() is not None:
        return
    os.killpg(CHILD.pid, signal.SIGTERM)
    try:
        CHILD.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(CHILD.pid, signal.SIGKILL)
        CHILD.wait(timeout=5)


def interrupted(signum, _frame):
    raise RuntimeError("interrupted by signal " + str(signum))


def run(argv, log, samples=None):
    global CHILD, SPAWN_UNCERTAIN
    if time.monotonic() >= DEADLINE or SPAWN_UNCERTAIN:
        raise RuntimeError("whole driver deadline")
    path = OUT / log
    with path.open("wb") as output:
        write("diagnostic-spawn-intent.json", {"argv": argv, "log": log, "wall_unix_ns": time.time_ns()})
        old_mask = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
        try:
            SPAWN_UNCERTAIN = True
            def restore_mask():
                signal.pthread_sigmask(signal.SIG_SETMASK, old_mask)
            CHILD = subprocess.Popen(argv, stdout=output, stderr=subprocess.STDOUT,
                                     start_new_session=True, preexec_fn=restore_mask)
            SPAWN_UNCERTAIN = False
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, old_mask)
        try:
            while CHILD.poll() is None:
                if time.monotonic() >= DEADLINE or path.stat().st_size > MAX_LOG:
                    raise RuntimeError("deadline/log limit")
                if samples is not None:
                    samples.append(snapshot(True))
                    if len(samples) > 310:
                        raise RuntimeError("sample ceiling")
                time.sleep(1 if samples is None else 5)
            if path.stat().st_size > MAX_LOG:
                raise RuntimeError("completed log over limit")
            return CHILD.returncode
        finally:
            stop_child()
            CHILD = None


def command(argv):
    result = subprocess.run(argv, capture_output=True, text=True, timeout=10, check=True)
    if len(result.stdout) + len(result.stderr) > 65536:
        raise RuntimeError("identity command output limit")
    return result.stdout.strip()


def tool_path(name):
    # The devShell does not include its invoking Nix executable in PATH.
    raw = os.environ["PERF_DIAGNOSTIC_NIX"] if name == "nix" else shutil.which(name)
    if not raw or not Path(raw).is_absolute() or not Path(raw).is_file():
        raise RuntimeError("missing absolute tool identity: " + name)
    return Path(raw).resolve()


def main():
    global TEST_EXIT
    if digest(__file__)["sha256"] != os.environ["PERF_DIAGNOSTIC_CONTROLLER_SHA256"]:
        raise RuntimeError("controller bytes changed")
    source = os.environ["PERF_DIAGNOSTIC_SOURCE"]
    if source not in SOURCES or command(["git", "rev-parse", "HEAD"]) != source:
        raise RuntimeError("unexpected source")
    if command(["git", "status", "--porcelain"]):
        raise RuntimeError("dirty benchmark source")
    os.environ["ST_BENCH_DIR"] = str(Path(os.environ["RUNNER_TEMP"]) / "st-bench")
    os.environ["TMPDIR"] = os.environ["RUNNER_TEMP"]
    os.environ["AGENT_SPEC_REVISION"] = source
    baseline_dir = Path(os.environ["RUNNER_TEMP"]) / "perf-baseline"
    baselines = {p.name: digest(p) for p in sorted(baseline_dir.glob("load-*.json"))}
    if len(baselines) != 5:
        raise RuntimeError("missing/overfull original baseline report set")
    identity = {"source": source, "tree": command(["git", "rev-parse", "HEAD^{tree}"]),
                "parents": command(["git", "show", "-s", "--format=%P", "HEAD"]),
                "runner": {k: os.environ.get(k) for k in
                           ["RUNNER_NAME", "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion",
                            "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT"]},
                "uname": command(["uname", "-a"]), "os_release": read("/etc/os-release"),
                "cpuinfo": read("/proc/cpuinfo"), "meminfo": read("/proc/meminfo"),
                "cgroup": read("/proc/self/cgroup"),
                "tools": {"cargo": command(["cargo", "-Vv"]),
                          "rustc": command(["rustc", "-Vv"]), "nix": command([str(tool_path("nix")), "--version"])},
                "tool_files": {name: {"resolved": str(tool_path(name)),
                                       **digest(tool_path(name))}
                               for name in ["cargo", "rustc", "nix", "python3"]},
                "fixture_hashes": {p: digest(p) for p in
                                   ["crates/st3/tests/daemon_load.rs", "crates/st3/tests/daemon_bench.rs",
                                    "crates/st3/tests/perf_load.rs", "Cargo.lock", "flake.lock"]},
                "restored_baselines": baselines,
                "physical_tenancy": "Unavailable; guest-visible capture is not physical CPU exclusivity proof."}
    write("diagnostic-inputs.json", identity)
    build_argv = BASE + ["--no-run", "--message-format=json"]
    build_exit = run(build_argv, "diagnostic-build.log")
    artifacts, finished = [], []
    for line in (OUT / "diagnostic-build.log").read_text().splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue  # stderr Cargo notices are retained verbatim, not treated as JSON.
        if event.get("reason") == "build-finished":
            finished.append(event.get("success"))
        if event.get("reason") == "compiler-artifact" and event.get("executable"):
            target = event.get("target", {})
            if target.get("name") == "perf_load" and "test" in target.get("kind", []):
                artifacts.append(event)
    if build_exit or finished != [True] or len(artifacts) != 1:
        raise RuntimeError("build/artifact qualification failed; no workload launched")
    artifact = artifacts[0]
    executable = artifact["executable"]
    if ("perf-load" not in artifact.get("features", [])
            or artifact.get("profile", {}).get("debug_assertions") is not False
            or artifact.get("profile", {}).get("test") is not True
            or Path(artifact["target"]["src_path"]).resolve() != (Path.cwd() / "crates/st3/tests/perf_load.rs").resolve()
            or Path(artifact["manifest_path"]).resolve() != (Path.cwd() / "crates/st3/Cargo.toml").resolve()):
        raise RuntimeError("artifact source/profile/features mismatch")
    write("diagnostic-artifact.json", {"cargo_event": artifact, "binary": digest(executable),
                                      "build_argv": build_argv, "build_exit": build_exit})
    # Let compiler/cache activity settle, then observe a bounded 10-second guest idle window.
    pre = snapshot(True)
    time.sleep(10)
    post = snapshot(True)
    idle = cpu_delta(pre, post)
    write("diagnostic-preflight.json", {"before": pre, "after": post, "cpu": idle,
                                       "scope": "Guest-visible idle only, not physical host proof."})
    compilers = [p for capture in [pre, post] for p in capture["processes"]
                 if (p.get("comm") or "").strip() in {"rustc", "rustdoc", "cargo", "nix-build"}]
    if idle["busy_fraction"] > .05 or idle["steal_fraction"] > .001 or compilers:
        raise RuntimeError("guest preflight not quiet; no measurement or automatic retry")
    os.environ["ST_LOAD_GATE"] = "1"
    os.environ["ST_LOAD_REPORT"] = str(OUT / "load.json")
    os.environ["ST_LOAD_BASELINE"] = str(baseline_dir)
    samples = [snapshot(True)]
    started = time.monotonic()
    cpu_started = time.process_time()
    argv = [executable, "--exact", TEST, "--nocapture", "--test-threads=1"]
    status = run(argv, "load.log", samples)
    TEST_EXIT = status
    samples.append(snapshot(True))
    write("diagnostic-load.json", {"argv": argv, "exit": status,
                                  "elapsed_seconds": time.monotonic() - started,
                                  "sampler_cpu_seconds": time.process_time() - cpu_started,
                                  "samples": samples, "cpu": cpu_delta(samples[0], samples[-1]),
                                  "physical_tenancy": "Unavailable; no unloaded physical-host claim."})
    (OUT / "load.time").write_text(str(time.monotonic() - started) + " " + str(status) + "\n")
    return status


if __name__ == "__main__":
    for sig in [signal.SIGINT, signal.SIGTERM]:
        signal.signal(sig, interrupted)
    outcome = 1
    try:
        outcome = main()
    except BaseException as error:
        write("diagnostic-error.json", {"error": str(error), "original_test_exit": TEST_EXIT,
                                       "elapsed_seconds": time.monotonic() - START})
    finally:
        stop_child()
        write("diagnostic-terminal.json", {"exit": outcome, "original_test_exit": TEST_EXIT,
                                           "spawn_uncertain": SPAWN_UNCERTAIN,
                                           "remaining_process_absence": "Not independently proved; hosted job teardown is required.",
                                           "sampler_process_cpu_seconds": time.process_time(),
                                           "max_rss_kib": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss})
    sys.exit(outcome)

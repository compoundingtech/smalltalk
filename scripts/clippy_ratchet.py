#!/usr/bin/env python3
"""Count workspace warning diagnostics from the two existing Linux Clippy runs."""
import argparse
from collections import Counter
import json
from pathlib import Path
import subprocess
import sys

BASELINE = ".github/clippy-baseline.json"
RUNS = ("clippy-workspace.jsonl", "clippy-model.jsonl")


def counts_from_messages(metadata, runs):
    members = set(metadata["workspace_members"])
    packages = {p["id"]: p["name"] for p in metadata["packages"] if p["id"] in members}
    seen = set()
    counts = Counter()
    for messages in runs:
        finished = False
        for item in messages:
            if finished:
                raise ValueError("diagnostics after build-finished")
            if item["reason"] == "build-finished":
                if not item["success"]:
                    raise ValueError("Clippy build failed")
                finished = True
            if item["reason"] != "compiler-message" or item["package_id"] not in packages:
                continue
            diagnostic = item["message"]
            if diagnostic["level"] == "error":
                raise ValueError("Clippy reported an error")
            if diagnostic["level"] != "warning":
                continue
            crate = packages[item["package_id"]]
            lint = (diagnostic["code"] or {}).get("code", "uncoded-warning")
            # Cargo repeats library diagnostics for lib/test targets and cached builds.
            # Count each source warning once, including across the standalone model run.
            spans = tuple(sorted((s["file_name"], s["byte_start"], s["byte_end"])
                                 for s in diagnostic["spans"] if s["is_primary"]))
            identity = (crate, lint, diagnostic["message"], spans)
            if identity not in seen:
                seen.add(identity)
                counts[(crate, lint)] += 1
        if not finished:
            raise ValueError("missing successful build-finished; rerun both Clippy commands")
    return {crate: {lint: count for (name, lint), count in sorted(counts.items()) if name == crate}
            for crate in sorted({name for name, _ in counts})}


def validate_baseline(value):
    if set(value) != {"version", "toolchain", "counts"} or value["version"] != 1:
        raise ValueError("invalid baseline format")
    if set(value["toolchain"]) != {"rustc", "clippy"} or not all(
            isinstance(v, str) and v for v in value["toolchain"].values()):
        raise ValueError("invalid baseline toolchain")
    for crate, lints in value["counts"].items():
        if not isinstance(crate, str) or not isinstance(lints, dict) or not lints:
            raise ValueError("invalid baseline crate")
        for lint, count in lints.items():
            if not isinstance(lint, str) or type(count) is not int or count <= 0:
                raise ValueError("baseline counts must be positive integers; omit zero counts")
    return value


def increases(candidate, ceiling):
    return [f"{crate}: {lint}: {ceiling.get(crate, {}).get(lint, 0)} -> {count}"
            for crate, lints in sorted(candidate.items()) for lint, count in sorted(lints.items())
            if count > ceiling.get(crate, {}).get(lint, 0)]


def baseline_at_ref(ref):
    # ls-tree distinguishes the initial rollout (file absent) from a bad/missing base ref.
    files = subprocess.run(["git", "ls-tree", "--name-only", ref, "--", BASELINE],
                           check=True, capture_output=True, text=True).stdout.strip()
    if not files:
        return None
    content = subprocess.run(["git", "show", f"{ref}:{BASELINE}"],
                             check=True, capture_output=True, text=True).stdout
    return validate_baseline(json.loads(content))


def check(observed, baseline, previous=None):
    failures = []
    if observed["toolchain"] != baseline["toolchain"]:
        failures.append("toolchain differs from baseline: run inside the flake.lock-pinned nix develop shell")
    if previous is not None:
        failures += ["baseline increased: " + row for row in increases(baseline["counts"], previous["counts"])]
    failures += ["new warning: " + row for row in increases(observed["counts"], baseline["counts"])]
    if increases(baseline["counts"], observed["counts"]):
        failures.append("warnings removed: lower the committed baseline with --update")
    if failures:
        raise ValueError("\n".join(failures))


def render(stream):
    for line in stream:
        item = json.loads(line)
        if item["reason"] == "compiler-message":
            message = item["message"].get("rendered")
            if message:
                print(message, end="", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--render", action="store_true", help="render Cargo JSON from stdin")
    parser.add_argument("--logs", type=Path, help="ci-logs directory from scripts/ci-linux clippy")
    parser.add_argument("--update", action="store_true", help="lower the baseline using these diagnostics")
    parser.add_argument("--base-ref", help="reject baseline increases relative to this Git revision")
    args = parser.parse_args()
    try:
        if args.render:
            render(sys.stdin)
            return 0
        if args.logs is None:
            parser.error("--logs is required")
        metadata = json.loads((args.logs / "clippy-metadata.json").read_text())
        runs = [[json.loads(line) for line in (args.logs / name).read_text().splitlines()]
                for name in RUNS]
        observed = {"version": 1, "toolchain": {
            "rustc": (args.logs / "clippy-rustc.txt").read_text().strip(),
            "clippy": (args.logs / "clippy-version.txt").read_text().strip(),
        }, "counts": counts_from_messages(metadata, runs)}
        validate_baseline(observed)
        path = Path(BASELINE)
        previous = baseline_at_ref(args.base_ref) if args.base_ref else None
        baseline = validate_baseline(json.loads(path.read_text())) if path.exists() else None
        if args.update:
            # Updating can never bless new debt, even outside CI.
            if baseline is not None and increases(observed["counts"], baseline["counts"]):
                raise ValueError("cannot update baseline upwards:\n" + "\n".join(
                    increases(observed["counts"], baseline["counts"])))
            check(observed, observed, previous)
            path.write_text(json.dumps(observed, indent=2, sort_keys=True) + "\n")
            baseline = observed
        if baseline is None:
            raise ValueError("missing committed Clippy baseline")
        check(observed, baseline, previous)
        total = sum(sum(lints.values()) for lints in observed["counts"].values())
        print(f"Clippy ratchet passed: {total} unique workspace warnings")
        for crate, lints in sorted(observed["counts"].items()):
            print(f"  {crate}: {sum(lints.values())}")
        return 0
    except (ValueError, KeyError, TypeError, AttributeError, OSError, subprocess.CalledProcessError) as error:
        print(f"Clippy ratchet failed: {error}", file=sys.stderr)
        print("After removing warnings, run: python3 scripts/clippy_ratchet.py --logs LOG_DIRECTORY --update\n"
              f"Commit {BASELINE} with the fix. See docs/ci.md#clippy-warning-ratchet.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

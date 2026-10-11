#!/usr/bin/env python3
"""Reconcile one tracked Git ref to an unattended, locally signed iOS Release."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import time

BUNDLE_ID = "com.compoundingtech.smalltalk"


def run(argv, *, cwd=None, capture=False, env=None, timeout=None):
    return subprocess.run(argv, cwd=cwd, env=env, check=True, text=True,
                          stdout=subprocess.PIPE if capture else None,
                          timeout=timeout).stdout


def git(repo, *args):
    return run(["git", "-C", str(repo), *args], capture=True, timeout=60).strip()


def local_packages(repo, commit):
    """Dependency-first closure of repository-local npm runtime dependencies."""
    found = []
    visiting = set()

    def visit(package):
        if package in found or package in visiting:
            return
        visiting.add(package)
        manifest = json.loads(git(repo, "show", f"{commit}:{package}/package.json"))
        for dependency in manifest.get("dependencies", {}).values():
            if dependency.startswith("file:"):
                resolved = os.path.normpath(f"{package}/{dependency[5:]}")
                if resolved.startswith("../") or os.path.isabs(resolved):
                    raise ValueError("Local dependency escapes the repository")
                visit(resolved)
        visiting.remove(package)
        found.append(package)

    visit("apps/ios")
    return found


def content_key(repo, commit, identity):
    """Identity of the signed app: app-affecting trees plus external build inputs."""
    trees = {package: git(repo, "rev-parse", f"{commit}:{package}")
             for package in local_packages(repo, commit)}
    return hashlib.sha256(json.dumps({"trees": trees, "inputs": identity},
                                     sort_keys=True).encode()).hexdigest()


def save(path, data):
    temporary = path.with_suffix(".new")
    temporary.write_text(json.dumps(data, indent=2) + "\n")
    temporary.replace(path)


# Environment read by the app's native and bundler configuration, pinned to
# the daily Release values so an inherited setting can never leak into a
# build: StFabric.podspec reads ST3_FABRIC_PROOF (Rust fabric module versus
# the disabled stub), metro.config.js reads ST3_FABRIC_OFFLINE_DEBUG, and the
# app config selects the daily bundle from APP_VARIANT.
DAILY_BUILD_ENV = {"APP_VARIANT": "daily", "ST3_FABRIC_PROOF": "0",
                   "ST3_FABRIC_OFFLINE_DEBUG": "0"}


def build_identity(args):
    """Inputs outside the Git revision that determine the signed app.

    The worker derives finer cache keys from these; the reconciler compares
    this digest so a changed toolchain or inherited public environment never
    trusts an artifact built from other inputs. The worker overrides
    EXPO_PUBLIC_ST3_BUILD from the revision, so an inherited value is not one.
    """
    identity = {
        "developer_dir": args.developer_dir,
        "tools": {name: shutil.which(name) for name in ("node", "npm", "npx", "pod")},
        "env": {key: value for key, value in sorted(os.environ.items())
                if key.startswith("EXPO_PUBLIC_") and key != "EXPO_PUBLIC_ST3_BUILD"},
        "pinned": DAILY_BUILD_ENV,
    }
    return hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()


def build_release(args):
    repo, output = args.repo, args.output
    app = repo / "apps/ios"
    cache = output.parent.parent / "build-cache"
    cache.mkdir(parents=True, exist_ok=True)
    inputs_file = cache / "inputs.json"
    inputs = json.loads(inputs_file.read_text()) if inputs_file.exists() else {}
    env = os.environ.copy()
    env.update(DAILY_BUILD_ENV, DEVELOPER_DIR=args.developer_dir,
               EXPO_PUBLIC_ST3_BUILD=args.commit[:12], CI="1",
               LANG="en_US.UTF-8", LC_ALL="en_US.UTF-8")
    # Metro inlines EXPO_PUBLIC_* values when it transforms each module, and
    # its transform cache is keyed by file content alone. Scope that cache to
    # this delivery and reset it whenever an inlined value changes, so an
    # unchanged module can never ship a previous revision's build stamp.
    tmp = cache / "tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(tmp)
    js_env = {key: value for key, value in env.items() if key.startswith("EXPO_PUBLIC_")}
    started = time.monotonic()
    packages = local_packages(repo, args.commit)
    identities = {}
    # Locked installations stay in the dedicated checkout. Reinstalling identical
    # node_modules changes native-header mtimes and defeats Xcode's build cache.
    for package in packages:
        identity = hashlib.sha256((git(repo, "ls-tree", args.commit, "--",
            f"{package}/package.json", f"{package}/package-lock.json")
            + str(shutil.which("node"))).encode()).hexdigest()
        identities[package] = identity
        if (inputs.get("packages", {}).get(package) != identity
                or not (repo / package / "node_modules").is_dir()):
            command = ["npm", "ci", "--no-audit", "--no-fund"]
            if package != "apps/ios":
                command.append("--ignore-scripts")
            run(command, cwd=repo / package, env=env)
            inputs.setdefault("packages", {})[package] = identity
            save(inputs_file, inputs)
        else:
            print(f"Reusing locked npm dependencies: {package}", flush=True)
    config = json.loads(run(["npx", "--no-install", "expo", "config", "--type", "public", "--json"],
                            cwd=app, env=env, capture=True))
    if config["ios"]["bundleIdentifier"] != BUNDLE_ID:
        raise ValueError("Tracked ref lacks the daily app variant; refusing to replace Debug")
    native_paths = ["apps/ios/app.json", "apps/ios/plugins", "apps/ios/modules",
                    "apps/ios/assets", "apps/ios/react-native.config.js"]
    native_paths.extend(str(path.relative_to(repo)) for path in app.glob("app.config.*"))
    native_identity = hashlib.sha256(json.dumps({
        "packages": identities, "config": config,
        "source": git(repo, "ls-tree", "-r", args.commit, "--", *native_paths),
        "developer_dir": args.developer_dir, "team": args.team,
        "pod": shutil.which("pod"), "env": DAILY_BUILD_ENV,
    }, sort_keys=True).encode()).hexdigest()
    regenerate = (inputs.get("native") != native_identity
                  or not list((app / "ios").glob("*.xcworkspace"))
                  or not (app / "ios/Podfile.lock").exists())
    if regenerate:
        run(["npx", "--no-install", "expo", "prebuild", "--platform", "ios", "--clean", "--no-install"],
            cwd=app, env=env)
        run(["pod", "install"], cwd=app / "ios", env=env)
        inputs = {"packages": identities, "native": native_identity}
        save(inputs_file, inputs)
    else:
        print("Reusing generated native project and CocoaPods", flush=True)
    workspaces = list((app / "ios").glob("*.xcworkspace"))
    if len(workspaces) != 1:
        raise ValueError("Expected one generated iOS workspace")
    workspace = workspaces[0]
    # Expo's generated plist is outside committed source. Stamp it before
    # Xcode signs the app; devicectl exposes the numeric build version (the
    # reconciler's monotonic counter) while StBuildCommit retains exact Git
    # provenance without adding proof-only UI.
    build_version = args.build_version
    plist = app / "ios" / workspace.stem / "Info.plist"
    with plist.open("rb") as file:
        bundle = plistlib.load(file)
    stamp = {"CFBundleVersion": build_version, "StBuildCommit": args.commit}
    if any(bundle.get(key) != value for key, value in stamp.items()):
        bundle.update(stamp)
        with plist.open("wb") as file:
            plistlib.dump(bundle, file)
    if inputs.get("js_env") != js_env:
        print("Resetting JS transform cache: build identity changed", flush=True)
        shutil.rmtree(tmp, ignore_errors=True)
        tmp.mkdir(parents=True, exist_ok=True)
    derived = cache / "derived"
    run(["xcodebuild", "-workspace", str(workspace), "-scheme", workspace.stem,
         "-configuration", "Release", "-sdk", "iphoneos", "-destination", "generic/platform=iOS",
         "-derivedDataPath", str(derived), "-jobs", "4",
         "CODE_SIGN_STYLE=Automatic", f"DEVELOPMENT_TEAM={args.team}",
         "CODE_SIGN_IDENTITY=Apple Development", "-allowProvisioningUpdates", "build"],
        cwd=app, env=env)
    apps = list((derived / "Build/Products/Release-iphoneos").glob("*.app"))
    if len(apps) != 1:
        raise ValueError("Expected one successful Release app")
    with (apps[0] / "Info.plist").open("rb") as file:
        info = plistlib.load(file)
    if info.get("CFBundleIdentifier") != BUNDLE_ID:
        raise ValueError("Built app is not the daily bundle")
    if any(info.get(key) != value for key, value in stamp.items()):
        raise ValueError("Built app has stale source identity")
    if args.commit[:12].encode() not in (apps[0] / "main.jsbundle").read_bytes():
        raise ValueError("Built app embeds a stale build stamp")
    run(["codesign", "--verify", "--deep", "--strict", str(apps[0])])
    # Pending installation owns an immutable snapshot, never the next build's
    # mutable Xcode product. A failure cannot publish or replace this snapshot.
    snapshot = output / apps[0].name
    if snapshot.exists():
        shutil.rmtree(snapshot)
    shutil.copytree(apps[0], snapshot, symlinks=True)
    inputs["js_env"] = js_env
    save(inputs_file, inputs)
    save(output / "success.json", {"commit": args.commit, "key": args.content_key, "app": str(snapshot),
        "build_version": build_version,
        "build_seconds": round(time.monotonic() - started, 3),
        "native_regenerated": regenerate})


class Delivery:
    def __init__(self, args):
        self.args = args
        self.state = args.state
        self.state.mkdir(parents=True, exist_ok=True)
        self.file = self.state / "state.json"
        self.data = json.loads(self.file.read_text()) if self.file.exists() else {}
        self.env = os.environ.copy()
        self.env["DEVELOPER_DIR"] = args.developer_dir

    def record(self, outcome, **fields):
        event = {"at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                 "outcome": outcome, **fields}
        line = json.dumps(event)
        print(line, flush=True)
        with (self.state / "outcomes.jsonl").open("a") as log:
            log.write(line + "\n")
        marker = None
        if outcome == "build_failed":
            marker = ("notified_build_failure", fields["commit"])
        elif outcome == "installed":
            marker = ("notified_install", fields["commit"])
        elif outcome == "built":
            self.data.pop("notified_build_failure", None)
            self.persist()
        elif outcome in ("install_failed", "install_deferred") and fields.get("reason") != "daily_app_running":
            unavailable_since = self.data.setdefault("unavailable_since", time.time())
            self.data["unavailable_operation"] = "install" if outcome == "install_failed" else "probe"
            self.persist()
            if time.time() - unavailable_since >= 3600:
                marker = ("notified_unavailable", True)
        if (marker and self.data.get(marker[0]) != marker[1]
                and self.args.notify_from and self.args.notify_to):
            # Notification errors must not discard an artifact or stop delivery.
            try:
                run(["st", "--daemon-wait", "0", "conversations", "send",
                     "--from", self.args.notify_from, self.args.notify_to,
                     "--subject", "iOS Release delivery", "--body", line], timeout=10)
                self.data[marker[0]] = marker[1]
                self.persist()
            except (OSError, subprocess.SubprocessError) as error:
                print(f"Notification failed: {error}", file=sys.stderr)

    def persist(self):
        save(self.file, self.data)

    def device(self, *argv):
        return run(["xcrun", "devicectl", *argv], env=self.env)

    def reconcile(self):
        config = {"device": self.args.device, "team": self.args.team}
        if self.data.get("config", config) != config:
            raise ValueError("Device/team changed: use a separate state directory")
        self.data["config"] = config
        repo = self.state / "checkout"
        if not repo.exists():
            run(["git", "clone", "--no-checkout", str(self.args.repo), str(repo)])
        # Fetch the source repo's origin directly, not its possibly stale tracking
        # refs. Local scratch refs also work, without pushing proof commits.
        if self.args.ref.startswith("origin/"):
            remote = git(self.args.repo, "remote", "get-url", "origin")
            ref = "refs/heads/" + self.args.ref[len("origin/"):]
        else:
            remote, ref = str(self.args.repo), self.args.ref
        # `observed` is the tracked head from the last successful fetch. Every
        # decision compares content keys against it, so reverts, app-irrelevant
        # commits, and changed build inputs need no separate bookkeeping.
        observed = self.data.get("observed")
        try:
            git(repo, "fetch", "--no-tags", remote, ref)
            head = git(repo, "rev-parse", "FETCH_HEAD^{commit}")
        except subprocess.SubprocessError as error:
            if not observed:
                raise
            # An internet outage must not block delivering the last observed
            # head to a reachable LAN phone. Never fall back to an older revision.
            self.record("fetch_failed", reason=str(error))
            head = observed["commit"]
            try:
                git(repo, "cat-file", "-e", head + "^{commit}")
            except subprocess.SubprocessError:
                self.record("deferred", commit=head, reason="observed_unavailable")
                return
        identity = build_identity(self.args)
        key = content_key(repo, head, identity)
        if not observed or observed["key"] != key:
            if observed and observed["inputs"] != identity:
                self.record("inputs_changed", commit=head)
            observed = {"commit": head, "key": key, "inputs": identity, "since": time.time()}
        elif observed["commit"] != head:
            self.record("irrelevant", commit=head)
            observed = {**observed, "commit": head}
        self.data["observed"] = observed
        self.persist()
        pending = self.data.get("pending")
        reason = None
        if pending and pending["key"] != key:
            reason = "superseded"
        elif pending and not pending["installed"] and self.downgrades(pending):
            reason = "downgrade"
        if reason:
            # A pending artifact for other content, or one that would lower
            # the phone's version, is never installed. If it already reached
            # the phone, the phone now holds its content.
            if pending["installed"]:
                self.mark_installed(pending)
            del self.data["pending"]
            self.persist()
            self.record("pending_discarded", commit=pending["commit"], reason=reason)
            pending = None
        if not pending:
            if self.data.get("installed_key") == key:
                self.record("unchanged", commit=head)
                return
            success = self.retained_artifact(key) or self.build(repo, head, key)
            if not success:
                return
            if self.downgrades(success):
                # Unreachable while versions come from the monotonic counter;
                # kept so no install path can bypass the version check.
                self.record("install_refused", commit=success["commit"], reason="downgrade",
                            build_version=success["build_version"])
                return
            pending = {**success, "since": observed["since"], "installed": False}
            self.data["pending"] = pending
            self.persist()
        if self.args.build_only:
            self.record("install_held", commit=pending["commit"], reason="build_only")
            return
        if not pending["installed"]:
            process_json = self.state / "processes.json"
            try:
                self.device("device", "info", "processes", "--device", self.args.device,
                            "--timeout", "15", "--json-output", str(process_json))
                processes = json.loads(process_json.read_text())["result"]["runningProcesses"]
            except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
                self.record("install_deferred", commit=pending["commit"], reason=str(error))
                return
            if self.data.get("unavailable_operation") == "probe":
                for key in ("unavailable_since", "notified_unavailable", "unavailable_operation"):
                    self.data.pop(key, None)
                self.persist()
            # Xcode 27 exposes executable/PID, not foreground status. Conservatively
            # treat a running daily app as active, but stop deferring at 40 minutes.
            running = any("/smalltalk.app/" in process.get("executable", "").lower() for process in processes)
            if running and time.time() - pending["since"] < self.args.max_deferral:
                self.record("install_deferred", commit=pending["commit"], reason="daily_app_running")
                return
            try:
                install_env = {**self.env, "AGENT_ACTION_APPROVAL": "deploy"}
                run(["xcrun", "devicectl", "device", "install", "app", "--device", self.args.device,
                     pending["app"], "--timeout", "90", "--json-output", str(self.state / "install.json")],
                    env=install_env)
            except (OSError, subprocess.SubprocessError) as error:
                self.record("install_failed", commit=pending["commit"], reason=str(error))
                return
            pending["installed"] = True
            for key in ("unavailable_since", "notified_unavailable", "unavailable_operation"):
                self.data.pop(key, None)
            self.persist()
            self.record("installed", commit=pending["commit"])
        try:
            run(["xcrun", "devicectl", "device", "process", "launch", "--device", self.args.device,
                 BUNDLE_ID, "--timeout", "30"],
                env={**self.env, "AGENT_ACTION_APPROVAL": "deploy"})
        except (OSError, subprocess.SubprocessError) as error:
            self.record("launch_failed", commit=pending["commit"], reason=str(error))
            return
        self.mark_installed(pending)
        del self.data["pending"]
        self.persist()
        self.record("launched", commit=self.data["installed"], elapsed_seconds=round(time.time() - pending["since"]))

    def mark_installed(self, artifact):
        self.data["installed"] = artifact["commit"]
        self.data["installed_key"] = artifact["key"]
        self.data["installed_version"] = artifact["build_version"]

    def downgrades(self, artifact):
        installed = self.data.get("installed_version")
        return installed is not None and int(artifact["build_version"]) < int(installed)

    def retained_artifact(self, key):
        """A successful snapshot of the same content, e.g. before a revert.

        Reuse never lowers the phone's CFBundleVersion: an older snapshot (or
        an unknown installed version) rebuilds at the observed head instead,
        which gets the next monotonic version.
        """
        installed = self.data.get("installed_version")
        for receipt in sorted((self.state / "artifacts").glob("*/success.json")):
            success = json.loads(receipt.read_text())
            if success.get("key") != key or not Path(success["app"]).is_dir():
                continue
            if installed is None or self.downgrades(success):
                self.record("reuse_skipped", commit=success["commit"],
                            build_version=success["build_version"], installed_version=installed)
                continue
            self.record("reused", commit=success["commit"])
            return success
        return None

    def build(self, repo, commit, key):
        gate = shutil.which("gate-slot")
        if not gate:
            raise ValueError("gate-slot is required for heavy-job admission")
        git(repo, "checkout", "--detach", commit)
        # CFBundleVersion must never decrease across refs, reverts, or branch
        # switches: the commit count alone is not monotonic, so successful
        # builds advance a persisted counter.
        assigned = max(int(self.data.get(field) or 0)
                       for field in ("last_build_version", "installed_version"))
        version = str(max(int(git(repo, "rev-list", "--count", commit)), assigned + 1))
        output = self.state / "artifacts" / commit
        # Keep partial compilation across failures. Only the success
        # receipt is invalidated; failed products are never installable.
        output.mkdir(parents=True, exist_ok=True)
        (output / "success.json").unlink(missing_ok=True)
        self.record("build_started", commit=commit, build_version=version)
        command = [gate, "--class", "heavy", "--max-hold", "2700", "--",
                   "taskpolicy", "-c", "utility", "nice", "-n", "19", sys.executable,
                   str(Path(__file__).resolve()), "--build-worker",
                   "--repo", str(repo), "--output", str(output),
                   "--team", self.args.team, "--commit", commit, "--content-key", key,
                   "--build-version", version, "--developer-dir", self.args.developer_dir]
        with (output / "build.log").open("a") as log:
            result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
        if result.returncode or not (output / "success.json").exists():
            self.record("build_failed", commit=commit, exit=result.returncode,
                        log=str(output / "build.log"))
            return None
        self.data["last_build_version"] = version
        self.persist()
        self.record("built", commit=commit, log=str(output / "build.log"))
        return json.loads((output / "success.json").read_text())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--ref", default=os.environ.get("ST_IOS_REF", "origin/main"))
    parser.add_argument("--state", type=Path)
    parser.add_argument("--device", default=os.environ.get("ST_IOS_DEVICE"))
    parser.add_argument("--team", default=os.environ.get("ST_IOS_TEAM"))
    parser.add_argument("--developer-dir", default=os.environ.get("DEVELOPER_DIR"))
    parser.add_argument("--notify-from", default=os.environ.get("ST_IOS_NOTIFY_FROM"))
    parser.add_argument("--notify-to", default=os.environ.get("ST_IOS_NOTIFY_TO"))
    parser.add_argument("--max-deferral", type=int,
                        default=int(os.environ.get("ST_IOS_MAX_DEFERRAL", "2400")))
    parser.add_argument("--build-only", action="store_true",
                        default=os.environ.get("ST_IOS_BUILD_ONLY") == "1",
                        help="Retain a signed artifact without touching the phone")
    parser.add_argument("--build-worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--output", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--commit", help=argparse.SUPPRESS)
    parser.add_argument("--content-key", help=argparse.SUPPRESS)
    parser.add_argument("--build-version", help=argparse.SUPPRESS)
    args = parser.parse_args()
    args.repo = args.repo.expanduser().resolve()
    if not args.team or not args.developer_dir:
        parser.error("--team/ST_IOS_TEAM and --developer-dir/DEVELOPER_DIR are required")
    if args.build_worker:
        build_release(args)
        return
    if not args.device or not args.state:
        parser.error("--device/ST_IOS_DEVICE and --state are required")
    args.state = args.state.expanduser().resolve()
    delivery = Delivery(args)
    with (args.state / "delivery.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            delivery.record("busy")
            return
        try:
            # Read after locking: another invocation may have just persisted.
            delivery.data = json.loads(delivery.file.read_text()) if delivery.file.exists() else {}
            delivery.reconcile()
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            delivery.record("error", reason=str(error))
            sys.exit(1)


if __name__ == "__main__":
    main()

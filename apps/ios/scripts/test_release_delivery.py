"""CLI integration tests using real Git repositories and executable tool fixtures.

Run with: python3 -m unittest discover -s apps/ios/scripts -p 'test_release_delivery.py'
No Apple tools, network access, or device are needed.
"""

import fcntl
import json
import os
from pathlib import Path
import plistlib
import random
import shutil
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest


SCRIPT = Path(__file__).with_name("release_delivery.py")
BUNDLE_ID = "com.compoundingtech.smalltalk"

# These are process-boundary fixtures, not replacements for delivery logic:
# Git, cloning, state persistence, subprocesses, and flock remain real.
COMMAND_FIXTURE = r'''
import json
import os
from pathlib import Path
import plistlib
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]
root = Path(os.environ["DELIVERY_FIXTURE_ROOT"])
control = json.loads((root / "control.json").read_text())
with (root / "commands.jsonl").open("a") as log:
    log.write(json.dumps({"command": name, "args": args, "cwd": os.getcwd(),
                          "developer_dir": os.environ.get("DEVELOPER_DIR"),
                          "native_env": {key: os.environ.get(key) for key in (
                              "APP_VARIANT", "ST3_FABRIC_PROOF", "ST3_FABRIC_OFFLINE_DEBUG")}}) + "\n")

if name == "gate-slot":
    command = args[args.index("--") + 1:]
    os.execvpe(command[0], command, os.environ)
if name == "taskpolicy":
    assert args[:2] == ["-c", "utility"], args
    os.execvpe(args[2], args[2:], os.environ)
if name == "nice":
    assert args[:2] == ["-n", "19"], args
    os.execvpe(args[2], args[2:], os.environ)
if name == "npm":
    expected = ["ci", "--no-audit", "--no-fund"]
    if Path.cwd().parts[-2:] != ("apps", "ios"):
        expected.append("--ignore-scripts")
    assert args == expected, args
    (Path.cwd() / "node_modules").mkdir(exist_ok=True)
    sys.exit(0)
if name == "npx":
    assert args[:2] == ["--no-install", "expo"], args
    if "config" in args:
        print(json.dumps({"name": control.get("expo_name", "Smalltalk"), "slug": "smalltalk", "ios": {
            "bundleIdentifier": "com.compoundingtech.smalltalk"}}))
    elif "prebuild" in args:
        (Path.cwd() / "ios" / "smalltalk.xcworkspace").mkdir(parents=True, exist_ok=True)
        (Path.cwd() / "ios" / "Podfile").write_text("# Fixture native project\n")
        native = Path.cwd() / "ios" / "smalltalk"
        native.mkdir(parents=True, exist_ok=True)
        with (native / "Info.plist").open("wb") as info:
            plistlib.dump({"CFBundleIdentifier": "com.compoundingtech.smalltalk",
                          "CFBundleExecutable": "smalltalk", "CFBundleVersion": "0",
                          "StBuildCommit": "unstamped"}, info)
    else:
        raise AssertionError(args)
    sys.exit(0)
if name == "codesign":
    assert args[:3] == ["--verify", "--deep", "--strict"], args
    sys.exit(control.get("codesign_exit", 0))
if name == "st":
    assert args[:4] == ["--daemon-wait", "0", "conversations", "send"], args
    assert args[args.index("--from") + 1] == "fixture.actor", args
    assert "fixture.recipient" in args, args
    assert "--body" in args, args
    sys.exit(control.get("notification_exit", 0))
if name == "pod":
    assert args[0] == "install", args
    (Path.cwd() / "Podfile.lock").write_text("PODS: fixture\n")
    sys.exit(0)
if name == "xcodebuild":
    assert "-derivedDataPath" in args, args
    output = Path(args[args.index("-derivedDataPath") + 1])
    build_exit = control.get("build_exit", 0)
    if not build_exit or control.get("artifact_on_failure", False):
        app = output / "Build" / "Products" / "Release-iphoneos" / "smalltalk.app"
        app.mkdir(parents=True, exist_ok=True)
        workspace = Path(args[args.index("-workspace") + 1])
        with (workspace.parent / workspace.stem / "Info.plist").open("rb") as template:
            bundle = plistlib.load(template)
        bundle["CFBundleIdentifier"] = control.get("bundle_id", bundle["CFBundleIdentifier"])
        if control.get("built_commit") is not None:
            bundle["StBuildCommit"] = control["built_commit"]
        if control.get("built_version") is not None:
            bundle["CFBundleVersion"] = control["built_version"]
        with (app / "Info.plist").open("wb") as info:
            plistlib.dump(bundle, info)
        (app / "smalltalk").write_bytes(os.environ["EXPO_PUBLIC_ST3_BUILD"].encode())
        # Records the external inputs the product was built with, so tests can
        # check installed content independently of the reconciler's own keys.
        (app / "inputs.json").write_text(json.dumps({
            "endpoint": os.environ.get("EXPO_PUBLIC_FIXTURE_ENDPOINT"),
            "developer_dir": os.environ.get("DEVELOPER_DIR")}))
        # Metro bakes EXPO_PUBLIC values into the bundle at transform time;
        # bundled_build simulates a transform cache serving a stale value.
        (app / "main.jsbundle").write_bytes(
            control.get("bundled_build", os.environ["EXPO_PUBLIC_ST3_BUILD"]).encode())
    print("fixture build output", flush=True)
    sys.exit(build_exit)
if name == "node":
    assert args in (["--version"], ["-v"]), args
    print("v24.0.0")
    sys.exit(0)
if name == "xcrun":
    assert args[0] == "devicectl", args
    result = {}
    status = 0
    if "processes" in args:
        result = {"runningProcesses": control.get("processes", [])}
        status = control.get("process_exit", 0)
    elif "install" in args:
        assert "app" in args, args
        status = control.get("install_exit", 0)
    elif "launch" in args:
        assert "com.compoundingtech.smalltalk" in args, args
        status = control.get("launch_exit", 0)
    else:
        raise AssertionError(args)
    if "--json-output" in args:
        target = Path(args[args.index("--json-output") + 1])
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps({"result": result}))
    if status:
        print("fixture device operation unavailable", file=sys.stderr)
    sys.exit(status)
raise AssertionError((name, args))
'''


class ReleaseDeliveryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="release-delivery-tests-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.source = self.root / "source"
        self.origin = self.root / "origin.git"
        self.state = self.root / "state"
        self.tools = self.root / "bin"
        self.tools.mkdir()
        self.control_path = self.root / "control.json"
        self.control_path.write_text("{}")
        self.command_log = self.root / "commands.jsonl"
        for name in ("gate-slot", "taskpolicy", "nice", "npm", "npx", "pod", "xcodebuild", "xcrun", "codesign", "st", "node"):
            executable = self.tools / name
            executable.write_text(f"#!{sys.executable}\n" + textwrap.dedent(COMMAND_FIXTURE))
            executable.chmod(0o755)
        self.env = dict(os.environ)
        self.env.update({
            "PATH": f"{self.tools}{os.pathsep}/usr/bin{os.pathsep}/bin",
            "DELIVERY_FIXTURE_ROOT": str(self.root),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_TERMINAL_PROMPT": "0",
            "ST_IOS_NOTIFY_FROM": "",
            "ST_IOS_NOTIFY_TO": "",
            "ST_IOS_MAX_DEFERRAL": "2400",
            "ST_IOS_BUILD_ONLY": "0",
        })
        self.git("init", "--bare", "--initial-branch=main", str(self.origin), cwd=self.root)
        self.git("init", "--initial-branch=main", str(self.source), cwd=self.root)
        self.git("config", "user.name", "Delivery Test")
        self.git("config", "user.email", "delivery-test@example.invalid")
        self.git("remote", "add", "origin", str(self.origin))
        packages = {
            "apps/ios": {"@smalltalk/st3-views": "file:../../clients/typescript/st3-views"},
            "clients/typescript/st3-views": {"@smalltalk/st3-client": "file:../st3-client"},
            "clients/typescript/st3-client": {},
        }
        for relative, dependencies in packages.items():
            self.write_source(f"{relative}/package.json", json.dumps({
                "name": Path(relative).name, "version": "1.0.0", "dependencies": dependencies,
            }))
            self.write_source(f"{relative}/package-lock.json", "{}")
            self.write_source(f"{relative}/index.ts", "export const fixture = 1;\n")
        self.write_source("apps/ios/app.json", json.dumps({"expo": {
            "name": "Smalltalk", "slug": "smalltalk", "ios": {"bundleIdentifier": BUNDLE_ID},
        }}))
        self.write_source("server/main.go", "package main\n")
        self.initial_commit = self.commit("Initial fixture")

    def git(self, *args, cwd=None):
        result = subprocess.run(
            ["git", *args], cwd=cwd or self.source, env=self.env,
            text=True, capture_output=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout.strip()

    def write_source(self, relative, contents):
        target = self.source / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(contents)

    def commit(self, message):
        self.git("add", "--all")
        self.git("commit", "-m", message)
        self.git("push", "origin", "main")
        return self.git("rev-parse", "HEAD")

    def configure(self, **values):
        control = json.loads(self.control_path.read_text())
        control.update(values)
        self.control_path.write_text(json.dumps(control))

    def invoke(self, *extra):
        return subprocess.run([
            sys.executable, str(SCRIPT), "--repo", str(self.source), "--ref", "origin/main",
            "--state", str(self.state), "--device", "TEST-DEVICE", "--team", "TESTTEAM",
            "--developer-dir", str(self.root / "Xcode.app" / "Contents" / "Developer"), *extra,
        ], cwd=self.root, env=self.env, text=True, capture_output=True, timeout=60)

    def reconcile(self, *extra):
        result = self.invoke(*extra)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def saved_state(self):
        path = self.state / "state.json"
        return json.loads(path.read_text()) if path.exists() else {}

    def outcomes(self):
        path = self.state / "outcomes.jsonl"
        return [json.loads(line)["outcome"] for line in path.read_text().splitlines()] if path.exists() else []

    def commands(self, name=None):
        entries = [json.loads(line) for line in self.command_log.read_text().splitlines()] if self.command_log.exists() else []
        return [entry for entry in entries if name is None or entry["command"] == name]

    def device_commands(self, action):
        return [entry for entry in self.commands("xcrun") if action in entry["args"]]

    def prebuild_commands(self):
        return [entry for entry in self.commands("npx") if "prebuild" in entry["args"]]

    def change_js(self, marker):
        self.write_source("apps/ios/index.ts", f"export const marker = {json.dumps(marker)};\n")
        return self.commit(f"JS change {marker}")

    def enable_notifications(self):
        self.env["ST_IOS_NOTIFY_FROM"] = "fixture.actor"
        self.env["ST_IOS_NOTIFY_TO"] = "fixture.recipient"

    def notifications(self):
        return [json.loads(entry["args"][entry["args"].index("--body") + 1])
                for entry in self.commands("st")]

    def age_unavailability(self):
        # Advance the persisted wall-clock deadline without a one-hour sleep or
        # replacing the production clock; all reconciliations remain real CLIs.
        data = self.saved_state()
        data["unavailable_since"] = time.time() - 3601
        (self.state / "state.json").write_text(json.dumps(data))

    def assert_installed(self, commit):
        self.assertEqual(self.saved_state().get("installed"), commit)
        self.assertFalse(self.saved_state().get("pending"))

    def test_app_and_recursive_local_packages_trigger_delivery(self):
        self.reconcile()
        self.assert_installed(self.initial_commit)
        for relative in (
            "apps/ios/index.ts",
            "clients/typescript/st3-views/index.ts",
            "clients/typescript/st3-client/index.ts",
        ):
            with self.subTest(path=relative):
                self.write_source(relative, f"export const changed = {json.dumps(relative)};\n")
                commit = self.commit(f"Change {relative}")
                self.reconcile()
                self.assert_installed(commit)
        self.assertEqual(len(self.commands("xcodebuild")), 4)
        self.assertEqual(len(self.device_commands("install")), 4)
        self.assertEqual(len(self.device_commands("launch")), 4)
        first_installs = self.commands("npm")[:3]
        self.assertEqual([Path(entry["cwd"]).relative_to(self.state / "checkout").as_posix()
                          for entry in first_installs], [
            "clients/typescript/st3-client", "clients/typescript/st3-views", "apps/ios",
        ])
        self.assertEqual(len(self.commands("gate-slot")), 4)
        self.assertEqual(len(self.commands("taskpolicy")), 4)
        self.assertEqual(len(self.commands("nice")), 4)

    def test_server_only_commit_is_skipped(self):
        self.reconcile()
        self.write_source("server/main.go", "package main\n// server-only change\n")
        commit = self.commit("Server-only change")
        self.reconcile()
        self.assertIn("irrelevant", self.outcomes())
        self.assertEqual(self.saved_state()["observed"]["commit"], commit)
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertEqual(len(self.device_commands("install")), 1)

    def test_dependency_path_prefix_does_not_include_sibling_package(self):
        self.reconcile()
        self.write_source("clients/typescript/st3-client-extras/index.ts", "export {};\n")
        self.commit("Unrelated sibling package")
        self.reconcile()
        self.assertIn("irrelevant", self.outcomes())
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)

    def test_failed_build_never_becomes_pending_or_installed(self):
        # A tool can leave output behind even when it exits unsuccessfully.
        self.configure(build_exit=65, artifact_on_failure=True)
        self.invoke()
        self.assertIn("build_failed", self.outcomes())
        self.assertNotIn("built", self.outcomes())
        self.assertFalse(self.saved_state().get("pending"))
        self.assertFalse(self.saved_state().get("installed"))
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertEqual(self.device_commands("install"), [])
        self.assertEqual(self.device_commands("launch"), [])
        output = self.state / "artifacts" / self.initial_commit
        self.assertFalse((output / "success.json").exists())
        self.assertIn("fixture build output", (output / "build.log").read_text())
        self.configure(build_exit=0)
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 2)

    def test_offline_install_retains_artifact_and_retry_does_not_rebuild(self):
        self.configure(install_exit=1)
        self.invoke()
        pending = self.saved_state()["pending"]
        self.assertEqual(pending["commit"], self.initial_commit)
        self.assertFalse(pending["installed"])
        artifact = Path(pending["app"])
        self.assertTrue((artifact / "Info.plist").is_file())
        self.assertFalse(self.saved_state().get("installed"))
        self.assertIn("built", self.outcomes())
        self.assertIn("install_failed", self.outcomes())
        self.assertEqual(self.device_commands("launch"), [])
        self.configure(install_exit=0)
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertTrue((artifact / "Info.plist").is_file())
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        installs = self.device_commands("install")
        self.assertEqual(len(installs), 2)
        self.assertTrue(all(str(artifact) in entry["args"] for entry in installs))
        self.assertEqual(len(self.device_commands("launch")), 1)

    def test_server_commit_does_not_discard_offline_pending_artifact(self):
        self.configure(install_exit=1)
        self.invoke()
        artifact = self.saved_state()["pending"]["app"]
        self.write_source("server/main.go", "package main\n// later server change\n")
        self.commit("Server change while device offline")
        self.configure(install_exit=0)
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertIn(artifact, self.device_commands("install")[-1]["args"])

    def test_singleflight_skips_when_lock_is_held(self):
        self.state.mkdir()
        lock = self.state / "delivery.lock"
        with lock.open("a+") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.reconcile()
            self.assertIn("busy", self.outcomes())
            self.assertEqual(self.commands(), [])
            self.assertFalse((self.state / "checkout").exists())
            self.assertFalse(self.saved_state().get("pending"))
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)

    def test_missing_gate_fails_closed_before_build(self):
        (self.tools / "gate-slot").unlink()
        self.invoke()
        self.assertTrue({"error", "build_failed"}.intersection(self.outcomes()), self.outcomes())
        self.assertEqual(self.commands("xcodebuild"), [])
        self.assertEqual(self.device_commands("install"), [])
        self.assertFalse(self.saved_state().get("pending"))
        self.assertFalse(self.saved_state().get("installed"))

    def test_invalid_bundle_or_signature_never_becomes_pending(self):
        for control in ({"bundle_id": "invalid.fixture.bundle", "codesign_exit": 0},
                        {"bundle_id": BUNDLE_ID, "codesign_exit": 1}):
            with self.subTest(control=control):
                self.configure(**control)
                self.invoke()
                self.assertFalse(self.saved_state().get("pending"))
                self.assertFalse(self.saved_state().get("installed"))
                self.assertEqual(self.device_commands("install"), [])
                self.assertEqual(self.device_commands("launch"), [])
                self.assertEqual(self.outcomes()[-1], "build_failed")
                self.assertFalse((self.state / "artifacts" / self.initial_commit / "success.json").exists())

    def test_failed_newer_build_preserves_previous_installed_release(self):
        self.reconcile()
        self.write_source("apps/ios/index.ts", "export const changed = true;\n")
        failed_commit = self.commit("New release that fails to build")
        self.configure(build_exit=65, artifact_on_failure=True)
        self.invoke()
        self.assert_installed(self.initial_commit)
        self.assertEqual(self.outcomes()[-1], "build_failed")
        self.assertEqual(len(self.commands("xcodebuild")), 2)
        self.assertEqual(len(self.device_commands("install")), 1)
        self.assertFalse((self.state / "artifacts" / failed_commit / "success.json").exists())

    def test_launch_failure_retries_launch_without_reinstall_or_rebuild(self):
        self.configure(launch_exit=1)
        self.invoke()
        pending = self.saved_state()["pending"]
        self.assertEqual(pending["commit"], self.initial_commit)
        self.assertTrue(pending["installed"])
        self.assertFalse(self.saved_state().get("installed"))
        self.assertIn("launch_failed", self.outcomes())
        self.configure(launch_exit=0)
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertEqual(len(self.device_commands("install")), 1)
        self.assertEqual(len(self.device_commands("launch")), 2)

    def test_running_daily_app_defers_install_until_deadline(self):
        self.configure(processes=[{
            "processIdentifier": 123,
            "executable": "/Applications/Smalltalk.app/Smalltalk",
        }])
        self.reconcile()
        self.assertIn("install_deferred", self.outcomes())
        self.assertEqual(self.saved_state()["pending"]["commit"], self.initial_commit)
        self.assertFalse(self.saved_state()["pending"]["installed"])
        self.assertEqual(self.device_commands("install"), [])
        # Exercise the real deadline through its configuration, without sleeps
        # or modifying timestamps in authoritative state.
        self.reconcile("--max-deferral", "0")
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertEqual(len(self.device_commands("install")), 1)

    def test_process_probe_failure_keeps_artifact_for_retry(self):
        self.configure(process_exit=1)
        self.invoke()
        pending = self.saved_state()["pending"]
        self.assertFalse(pending["installed"])
        self.assertTrue((Path(pending["app"]) / "Info.plist").exists())
        self.assertIn("install_deferred", self.outcomes())
        self.assertEqual(self.device_commands("install"), [])
        self.configure(process_exit=0)
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)

    def test_unchanged_commit_does_not_build_or_install_again(self):
        self.reconcile()
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(self.outcomes()[-1], "unchanged")
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertEqual(len(self.device_commands("install")), 1)
        self.assertEqual(len(self.device_commands("launch")), 1)

    def test_delivery_uses_dedicated_clone_without_changing_source_worktree(self):
        dirty_contents = "export const uncommitted = true;\n"
        self.write_source("apps/ios/index.ts", dirty_contents)
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(self.git("branch", "--show-current"), "main")
        self.assertEqual((self.source / "apps/ios/index.ts").read_text(), dirty_contents)
        self.assertEqual(self.git("rev-parse", "HEAD"), self.initial_commit)
        checkout = self.state / "checkout"
        self.assertEqual(self.git("rev-parse", "HEAD", cwd=checkout), self.initial_commit)
        self.assertEqual((checkout / "apps/ios/index.ts").read_text(), "export const fixture = 1;\n")

    def test_recursive_dependency_scope_recomputes_when_graph_changes(self):
        self.reconcile()
        package = {
            "name": "st3-views", "version": "1.0.0",
            "dependencies": {"new-local-package": "file:../new-local-package"},
        }
        self.write_source("clients/typescript/st3-views/package.json", json.dumps(package))
        self.write_source("clients/typescript/new-local-package/package.json",
                          json.dumps({"name": "new-local-package", "version": "1.0.0"}))
        self.write_source("clients/typescript/new-local-package/package-lock.json", "{}")
        self.write_source("clients/typescript/new-local-package/index.ts", "export const fixture = 1;\n")
        graph_commit = self.commit("Retarget transitive local dependency")
        self.reconcile()
        self.assert_installed(graph_commit)
        self.write_source("clients/typescript/new-local-package/index.ts", "export const fixture = 2;\n")
        changed_commit = self.commit("Change new transitive local dependency")
        self.reconcile()
        self.assert_installed(changed_commit)
        self.write_source("clients/typescript/st3-client/index.ts", "export const obsolete = true;\n")
        self.commit("Change package removed from dependency graph")
        self.reconcile()
        self.assert_installed(changed_commit)
        self.assertEqual(self.outcomes()[-1], "unchanged")
        self.assertIn("irrelevant", self.outcomes())
        self.assertEqual(len(self.commands("xcodebuild")), 3)

    def test_build_only_holds_phone_and_later_resumes_without_rebuilding(self):
        self.reconcile("--build-only")
        pending = self.saved_state()["pending"]
        self.assertEqual(pending["commit"], self.initial_commit)
        self.assertFalse(pending["installed"])
        artifact = Path(pending["app"])
        self.assertTrue((artifact / "Info.plist").is_file())
        self.assertEqual(self.outcomes()[-1], "install_held")
        self.assertEqual(self.commands("xcrun"), [])
        self.assertFalse(self.saved_state().get("installed"))
        self.reconcile("--build-only")
        self.assertEqual(self.outcomes()[-1], "install_held")
        self.assertEqual(self.commands("xcrun"), [])
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertEqual(len(self.device_commands("install")), 1)
        self.assertIn(str(artifact), self.device_commands("install")[0]["args"])
        self.assertEqual(len(self.device_commands("launch")), 1)

    def test_fetch_failure_does_not_block_retrying_successful_pending_artifact(self):
        self.configure(install_exit=1)
        self.invoke()
        pending = self.saved_state()["pending"]
        artifact = pending["app"]
        self.assertEqual(pending["commit"], self.initial_commit)
        self.git("remote", "set-url", "origin", str(self.root / "unreachable.git"))
        self.configure(install_exit=0)
        self.reconcile()
        self.assertIn("fetch_failed", self.outcomes())
        self.assert_installed(self.initial_commit)
        self.assertEqual(self.saved_state()["observed"]["commit"], self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertEqual(len(self.device_commands("install")), 2)
        self.assertIn(artifact, self.device_commands("install")[-1]["args"])
        self.assertTrue((Path(artifact) / "Info.plist").is_file())

    def test_fetch_failure_never_installs_artifact_superseded_by_failed_newer_build(self):
        self.configure(install_exit=1)
        self.invoke()
        superseded = self.saved_state()["pending"]
        self.assertEqual(superseded["commit"], self.initial_commit)
        newer = self.change_js("superseding-release")
        self.configure(build_exit=65)
        self.invoke()
        self.assertEqual(self.outcomes()[-1], "build_failed")
        self.assertEqual(self.saved_state()["observed"]["commit"], newer)
        self.assertIn("pending_discarded", self.outcomes())
        self.git("remote", "set-url", "origin", str(self.root / "unreachable.git"))
        # Offline, the last observed revision is retried from the local
        # checkout; while it keeps failing the superseded artifact is never installed.
        self.configure(install_exit=0)
        self.invoke()
        self.assertEqual(self.outcomes()[-3:], ["fetch_failed", "build_started", "build_failed"])
        self.assertEqual(self.saved_state()["observed"]["commit"], newer)
        self.assertFalse(self.saved_state().get("pending"))
        self.assertFalse(self.saved_state().get("installed"))
        self.assertEqual(len(self.device_commands("install")), 1)
        self.assertEqual(self.device_commands("launch"), [])
        # Once the newer revision builds it delivers, even while offline.
        self.configure(install_exit=0, build_exit=0)
        self.reconcile()
        self.assert_installed(newer)
        self.assertEqual(len(self.commands("xcodebuild")), 4)

    def test_fetch_failure_with_changed_inputs_rebuilds_desired_not_superseded_revision(self):
        self.configure(install_exit=1)
        self.invoke()
        self.assertEqual(self.saved_state()["pending"]["commit"], self.initial_commit)
        newer = self.change_js("superseding-release")
        self.configure(build_exit=65)
        self.invoke()
        self.assertEqual(self.saved_state()["observed"]["commit"], newer)
        self.git("remote", "set-url", "origin", str(self.root / "unreachable.git"))
        self.env["EXPO_PUBLIC_FIXTURE_ENDPOINT"] = "changed-offline"
        self.configure(install_exit=0, build_exit=0)
        self.reconcile()
        self.assertIn("fetch_failed", self.outcomes())
        self.assertIn("inputs_changed", self.outcomes())
        self.assertEqual(self.saved_state()["observed"]["commit"], newer)
        self.assert_installed(newer)
        self.assertEqual(len(self.device_commands("install")), 2)
        self.assertTrue(any(f"artifacts/{newer}/" in arg
                            for arg in self.device_commands("install")[-1]["args"]))

    def test_fetched_revert_supersedes_failed_revision(self):
        self.reconcile()
        self.change_js("reverted-release")
        self.configure(build_exit=65)
        self.invoke()
        self.assertEqual(self.outcomes()[-1], "build_failed")
        revert = self.revert_head()
        self.reconcile()
        self.assertEqual(self.saved_state()["observed"]["commit"], revert)
        self.assertEqual(self.outcomes()[-1], "unchanged")
        # Offline ticks keep the revert as the observed head: the failed
        # revision is never rebuilt or installed.
        self.git("remote", "set-url", "origin", str(self.root / "unreachable.git"))
        self.configure(build_exit=0)
        self.reconcile()
        self.assertEqual(self.outcomes()[-2:], ["fetch_failed", "unchanged"])
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 2)
        self.assertEqual(len(self.device_commands("install")), 1)

    def installed_build_version(self):
        app = Path(next(arg for arg in self.device_commands("install")[-1]["args"] if arg.endswith(".app")))
        with (app / "Info.plist").open("rb") as info:
            return int(plistlib.load(info)["CFBundleVersion"])

    def test_revert_never_reuses_artifact_older_than_installed_version(self):
        self.reconcile()
        second = self.change_js("second-release")
        self.reconcile()
        self.assert_installed(second)
        installed_version = self.installed_build_version()
        revert = self.revert_head()
        self.reconcile()
        # The initial snapshot has the same content but a lower CFBundleVersion:
        # it is rebuilt at the observed head rather than installed as a downgrade.
        self.assertIn("reuse_skipped", self.outcomes())
        self.assertNotIn("reused", self.outcomes())
        self.assert_installed(revert)
        self.assertEqual(len(self.commands("xcodebuild")), 3)
        self.assertEqual(len(self.device_commands("install")), 3)
        self.assertGreater(self.installed_build_version(), installed_version)
        self.assertEqual(self.saved_state()["installed_version"], str(self.installed_build_version()))

    def test_revert_reuses_matching_artifact_newer_than_installed_version(self):
        self.reconcile()
        self.configure(install_exit=1)
        retained = self.change_js("retained-release")
        self.invoke()
        self.assertEqual(self.saved_state()["pending"]["commit"], retained)
        self.change_js("failing-release")
        self.configure(build_exit=65)
        self.invoke()
        self.assertEqual(self.outcomes()[-1], "build_failed")
        self.revert_head()
        self.configure(install_exit=0, build_exit=0)
        self.reconcile()
        # The revert restores retained content above the installed version.
        self.assertIn("reused", self.outcomes())
        self.assert_installed(retained)
        self.assertEqual(len(self.commands("xcodebuild")), 3)
        self.assertTrue(any(f"artifacts/{retained}/" in arg
                            for arg in self.device_commands("install")[-1]["args"]))

    def test_branch_switch_to_older_revision_never_lowers_build_version(self):
        self.reconcile()
        second = self.change_js("two-commit-release")
        self.reconcile()
        self.assert_installed(second)
        installed_version = self.installed_build_version()
        self.assertEqual(installed_version, 2)
        # A supported local branch at the initial app revision: its retained
        # artifact (version 1) is skipped, and its rebuild must not reuse the
        # lower commit count either.
        self.git("branch", "older-app", self.initial_commit)
        self.reconcile("--ref", "older-app")
        self.assertIn("reuse_skipped", self.outcomes())
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 3)
        self.assertEqual(self.installed_build_version(), 3)
        self.assertEqual(self.saved_state()["installed_version"], "3")
        receipt = json.loads((self.state / "artifacts" / self.initial_commit / "success.json").read_text())
        self.assertEqual(receipt["build_version"], "3")
        # Switching back advances again instead of reusing version 2.
        self.reconcile()
        self.assert_installed(second)
        self.assertEqual(self.installed_build_version(), 4)

    def test_inherited_native_environment_is_pinned_to_daily_release_values(self):
        self.env.update(ST3_FABRIC_PROOF="1", ST3_FABRIC_OFFLINE_DEBUG="1", APP_VARIANT="dev")
        self.reconcile()
        native = [entry["native_env"] for entry in self.commands()
                  if entry["command"] in ("npx", "pod", "xcodebuild")]
        self.assertTrue(native)
        for observed in native:
            self.assertEqual(observed, {"APP_VARIANT": "daily", "ST3_FABRIC_PROOF": "0",
                                        "ST3_FABRIC_OFFLINE_DEBUG": "0"})
        # The inherited values are not build inputs, so dropping them neither
        # rebuilds nor reinstalls.
        for key in ("ST3_FABRIC_PROOF", "ST3_FABRIC_OFFLINE_DEBUG", "APP_VARIANT"):
            del self.env[key]
        self.reconcile()
        self.assertEqual(self.outcomes()[-1], "unchanged")
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertEqual(len(self.device_commands("install")), 1)

    def revert_head(self):
        self.git("revert", "--no-edit", "HEAD")
        self.git("push", "origin", "main")
        return self.git("rev-parse", "HEAD")

    def app_trees(self, commit):
        return [self.git("rev-parse", f"{commit}:{package}") for package in (
            "apps/ios", "clients/typescript/st3-views", "clients/typescript/st3-client")]

    def reset_delivery(self):
        """Fresh delivery state and fault controls for each randomized sequence."""
        shutil.rmtree(self.state, ignore_errors=True)
        self.control_path.write_text("{}")
        self.command_log.unlink(missing_ok=True)

    def test_random_sequences_install_only_last_observed_content(self):
        """Randomized fault sequences; replay a failure with its printed seed."""
        for seed in range(3):
            with self.subTest(seed=seed):
                self.reset_delivery()
                rng = random.Random(seed)
                developer_dir = str(self.root / "Xcode.app" / "Contents" / "Developer")
                observed = None
                phone = None
                endpoints = ["alpha", "beta"]
                self.env["EXPO_PUBLIC_FIXTURE_ENDPOINT"] = "alpha"
                trace = []
                for step in range(14):
                    change = rng.choice(["none", "app", "irrelevant", "revert", "inputs"])
                    if change == "app":
                        self.change_js(f"seed-{seed}-step-{step}")
                    elif change == "irrelevant":
                        self.write_source("server/main.go", f"package main\n// {seed}-{step}\n")
                        self.commit(f"Server change {step}")
                    elif change == "revert" and self.git("rev-list", "--count", "HEAD") != "1":
                        self.revert_head()
                    elif change == "inputs":
                        self.env["EXPO_PUBLIC_FIXTURE_ENDPOINT"] = rng.choice(endpoints)
                    fetch_ok = observed is None or rng.random() < 0.7
                    faults = {"build_exit": 0 if rng.random() < 0.7 else 65,
                              "install_exit": 0 if rng.random() < 0.7 else 1,
                              "launch_exit": 0 if rng.random() < 0.8 else 1}
                    self.configure(**faults)
                    if not fetch_ok:
                        self.git("remote", "set-url", "origin", str(self.root / "unreachable.git"))
                    installs = len(self.device_commands("install"))
                    self.invoke()
                    self.git("remote", "set-url", "origin", str(self.origin))
                    if fetch_ok:
                        observed = self.git("rev-parse", "HEAD")
                    expected = (self.app_trees(observed),
                                {"endpoint": self.env["EXPO_PUBLIC_FIXTURE_ENDPOINT"],
                                 "developer_dir": developer_dir})
                    trace.append((step, change, fetch_ok, faults, self.outcomes()[-4:]))
                    message = f"seed={seed} trace={trace}"
                    new_installs = self.device_commands("install")[installs:]
                    self.assertLessEqual(len(new_installs), 1, message)
                    if new_installs:
                        app = Path(next(arg for arg in new_installs[0]["args"] if arg.endswith(".app")))
                        built = self.git("rev-parse", (app / "smalltalk").read_text())
                        content = (self.app_trees(built), json.loads((app / "inputs.json").read_text()))
                        # Every install carries the most recently observed content.
                        self.assertEqual(content, expected, message)
                        if faults["install_exit"] == 0:
                            phone = content
                    if not any(faults.values()):
                        # A healthy tick always converges the phone; it never
                        # stays stuck behind a matching or superseded artifact.
                        self.assertEqual(phone, expected, message)
                        self.assert_installed(self.saved_state()["installed"])

    def test_changed_build_inputs_rebuild_unchanged_revision_before_trusting_artifacts(self):
        self.reconcile("--build-only")
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        # Unchanged inputs keep no-rebuild retries, including an inherited
        # build stamp that the worker always overrides from the revision.
        self.env["EXPO_PUBLIC_ST3_BUILD"] = "inherited-ignored"
        self.reconcile("--build-only")
        self.assertEqual(self.outcomes()[-1], "install_held")
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        # A pending artifact built from other inputs is never installed.
        self.env["EXPO_PUBLIC_FIXTURE_ENDPOINT"] = "changed"
        self.reconcile()
        self.assertIn("inputs_changed", self.outcomes())
        self.assertEqual(len(self.commands("xcodebuild")), 2)
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.device_commands("install")), 1)
        self.reconcile()
        self.assertEqual(self.outcomes()[-1], "unchanged")
        self.assertEqual(len(self.commands("xcodebuild")), 2)
        # An installed artifact is rebuilt and redelivered after a toolchain change.
        self.reconcile("--developer-dir", str(self.root / "OtherXcode/Developer"))
        self.assertEqual(len(self.commands("xcodebuild")), 3)
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.device_commands("install")), 2)
        # A pending artifact whose rebuild for changed inputs fails is never
        # installed, on that tick or on later retries with the same inputs.
        other_xcode = str(self.root / "OtherXcode/Developer")
        self.env["EXPO_PUBLIC_FIXTURE_ENDPOINT"] = "pending-before-change"
        self.reconcile("--build-only", "--developer-dir", other_xcode)
        self.assertEqual(self.saved_state()["pending"]["commit"], self.initial_commit)
        self.env["EXPO_PUBLIC_FIXTURE_ENDPOINT"] = "changed-again"
        self.configure(build_exit=65)
        for _ in range(2):
            self.invoke("--developer-dir", other_xcode)
            self.assertEqual(self.outcomes()[-1], "build_failed")
        self.assertEqual(len(self.device_commands("install")), 2)
        self.assertFalse(self.saved_state().get("pending"))
        self.assertIn("pending_discarded", self.outcomes())
        self.configure(build_exit=0)
        self.reconcile("--developer-dir", other_xcode)
        self.assertEqual(len(self.commands("xcodebuild")), 7)
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.device_commands("install")), 3)

    def test_notifications_only_report_new_commit_install_success(self):
        self.enable_notifications()
        self.reconcile("--build-only")
        self.assertEqual(self.notifications(), [])
        self.reconcile()
        notices = self.notifications()
        self.assertEqual([event["outcome"] for event in notices], ["installed"])
        self.assertEqual(notices[0]["commit"], self.initial_commit)
        self.reconcile()
        self.write_source("server/main.go", "package main\n// unrelated notification tick\n")
        self.commit("Unrelated notification tick")
        self.reconcile()
        self.assertEqual(self.notifications(), notices)
        self.write_source("apps/ios/index.ts", "export const notification = true;\n")
        next_commit = self.commit("New install notification")
        self.reconcile()
        self.assertEqual([event["outcome"] for event in self.notifications()], ["installed", "installed"])
        self.assertEqual(self.notifications()[-1]["commit"], next_commit)

    def test_build_failure_notifies_once_per_commit_and_clears_on_recovery(self):
        self.enable_notifications()
        self.configure(build_exit=65)
        self.invoke()
        self.invoke()
        self.assertEqual([event["outcome"] for event in self.notifications()], ["build_failed"])
        self.assertEqual(self.saved_state()["notified_build_failure"], self.initial_commit)
        self.write_source("apps/ios/index.ts", "export const anotherFailure = true;\n")
        next_commit = self.commit("Another failing release")
        self.invoke()
        notices = self.notifications()
        self.assertEqual([event["commit"] for event in notices], [self.initial_commit, next_commit])
        self.assertEqual([event["outcome"] for event in notices], ["build_failed", "build_failed"])
        self.configure(build_exit=0)
        self.reconcile()
        self.assert_installed(next_commit)
        self.assertNotIn("notified_build_failure", self.saved_state())
        self.assertEqual([event["outcome"] for event in self.notifications()],
                         ["build_failed", "build_failed", "installed"])
        self.reconcile()
        self.assertEqual(len(self.notifications()), 3)

    def test_process_unavailability_notifies_after_one_hour_once_until_recovery(self):
        self.enable_notifications()
        self.configure(process_exit=1)
        self.invoke()
        self.assertIn("unavailable_since", self.saved_state())
        self.assertEqual(self.notifications(), [])
        self.age_unavailability()
        self.invoke()
        self.invoke()
        self.assertEqual([event["outcome"] for event in self.notifications()], ["install_deferred"])
        self.assertTrue(self.saved_state()["notified_unavailable"])
        self.configure(process_exit=0)
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertNotIn("unavailable_since", self.saved_state())
        self.assertNotIn("notified_unavailable", self.saved_state())
        self.assertEqual([event["outcome"] for event in self.notifications()], ["install_deferred", "installed"])
        self.assertEqual(len(self.commands("xcodebuild")), 1)

    def test_install_unavailability_timer_survives_successful_process_probes(self):
        self.enable_notifications()
        self.configure(install_exit=1)
        self.invoke()
        self.assertEqual(self.notifications(), [])
        self.age_unavailability()
        aged_since = self.saved_state()["unavailable_since"]
        self.invoke()
        self.invoke()
        self.assertEqual(self.saved_state()["unavailable_since"], aged_since)
        self.assertEqual([event["outcome"] for event in self.notifications()], ["install_failed"])
        self.assertTrue(self.saved_state()["notified_unavailable"])
        self.configure(install_exit=0)
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertNotIn("unavailable_since", self.saved_state())
        self.assertNotIn("notified_unavailable", self.saved_state())
        self.assertEqual([event["outcome"] for event in self.notifications()], ["install_failed", "installed"])
        self.assertEqual(len(self.commands("xcodebuild")), 1)

    def test_js_only_changes_reuse_native_cache_but_reset_inlined_js_transforms(self):
        self.reconcile("--build-only")
        cache = self.state / "build-cache"
        first_inputs = json.loads((cache / "inputs.json").read_text())
        derived = cache / "derived"
        sentinel = derived / "incremental-cache-sentinel"
        sentinel.write_text("keep incremental outputs")
        for relative in ("apps/ios/index.ts", "clients/typescript/st3-client/index.ts"):
            with self.subTest(path=relative):
                transforms = cache / "tmp" / "metro-transform-cache"
                transforms.mkdir(parents=True, exist_ok=True)
                transform_sentinel = transforms / "sentinel"
                transform_sentinel.write_text("inlined build identity")
                self.write_source(relative, f"export const changed = {json.dumps(relative)};\n")
                commit = self.commit(f"JS-only change {relative}")
                self.reconcile("--build-only")
                self.assertEqual(self.saved_state()["pending"]["commit"], commit)
                after = json.loads((cache / "inputs.json").read_text())
                self.assertEqual(after["packages"], first_inputs["packages"])
                self.assertEqual(after["native"], first_inputs["native"])
                self.assertEqual(after["js_env"], {"EXPO_PUBLIC_ST3_BUILD": commit[:12]})
                # A changed stamp resets the scoped transform cache while the
                # native incremental cache stays warm.
                self.assertFalse(transform_sentinel.exists())
                self.assertEqual(sentinel.read_text(), "keep incremental outputs")
                success = json.loads((self.state / "artifacts" / commit / "success.json").read_text())
                self.assertFalse(success["native_regenerated"])
        self.assertEqual(len(self.commands("npm")), 3)
        self.assertEqual(len(self.prebuild_commands()), 1)
        self.assertEqual(len(self.commands("pod")), 1)
        builds = self.commands("xcodebuild")
        self.assertEqual(len(builds), 3)
        self.assertEqual({entry["args"][entry["args"].index("-derivedDataPath") + 1]
                          for entry in builds}, {str(derived)})
        self.assertEqual(self.device_commands("install"), [])

    def test_package_manifest_and_lock_changes_invalidate_only_changed_package(self):
        self.reconcile("--build-only")
        changes = (
            ("clients/typescript/st3-client/package.json",
             json.dumps({"name": "st3-client", "version": "2.0.0", "dependencies": {}})),
            ("apps/ios/package-lock.json", '{"lockfileVersion": 3}'),
        )
        for count, (relative, contents) in enumerate(changes, start=1):
            with self.subTest(path=relative):
                self.write_source(relative, contents)
                commit = self.commit(f"Update dependency input {relative}")
                self.reconcile("--build-only")
                installs = self.commands("npm")
                self.assertEqual(len(installs), 3 + count)
                self.assertEqual(Path(installs[-1]["cwd"]),
                                 self.state / "checkout" / Path(relative).parent)
                self.assertEqual(len(self.prebuild_commands()), 1 + count)
                self.assertEqual(len(self.commands("pod")), 1 + count)
                self.assertEqual(self.saved_state()["pending"]["commit"], commit)

    def test_native_config_plugins_modules_and_assets_invalidate_generation(self):
        self.reconcile("--build-only")
        changes = (
            ("apps/ios/app.json", json.dumps({"expo": {"name": "Changed"}})),
            ("apps/ios/app.config.js", "module.exports = { name: 'Changed' };\n"),
            ("apps/ios/plugins/fixture.js", "module.exports = config => config;\n"),
            ("apps/ios/modules/fixture/NativeModule.m", "// native input\n"),
            ("apps/ios/assets/fixture.txt", "native asset\n"),
            ("apps/ios/react-native.config.js", "module.exports = {};\n"),
        )
        for count, (relative, contents) in enumerate(changes, start=1):
            with self.subTest(path=relative):
                self.write_source(relative, contents)
                commit = self.commit(f"Update native input {relative}")
                self.reconcile("--build-only")
                self.assertEqual(len(self.commands("npm")), 3)
                self.assertEqual(len(self.prebuild_commands()), 1 + count)
                self.assertEqual(len(self.commands("pod")), 1 + count)
                self.assertEqual(self.saved_state()["pending"]["commit"], commit)

    def test_evaluated_expo_config_change_invalidates_native_cache(self):
        self.reconcile("--build-only")
        self.configure(expo_name="Runtime config changed")
        self.change_js("evaluated-config")
        self.reconcile("--build-only")
        self.assertEqual(len(self.commands("npm")), 3)
        self.assertEqual(len(self.prebuild_commands()), 2)
        self.assertEqual(len(self.commands("pod")), 2)

    def test_missing_node_modules_workspace_and_pod_lock_regenerate_required_inputs(self):
        self.reconcile("--build-only")
        checkout = self.state / "checkout"
        shutil.rmtree(checkout / "clients/typescript/st3-client/node_modules")
        self.change_js("missing-node-modules")
        self.reconcile("--build-only")
        self.assertEqual(len(self.commands("npm")), 4)
        self.assertEqual(Path(self.commands("npm")[-1]["cwd"]), checkout / "clients/typescript/st3-client")
        self.assertEqual(len(self.prebuild_commands()), 1)
        (checkout / "apps/ios/ios/smalltalk.xcworkspace").rmdir()
        self.change_js("missing-workspace")
        self.reconcile("--build-only")
        self.assertEqual(len(self.prebuild_commands()), 2)
        (checkout / "apps/ios/ios/Podfile.lock").unlink()
        self.change_js("missing-pod-lock")
        self.reconcile("--build-only")
        self.assertEqual(len(self.prebuild_commands()), 3)
        self.assertEqual(len(self.commands("pod")), 3)
        self.assertEqual(len(self.commands("npm")), 4)

    def test_toolchain_path_changes_invalidate_their_cache_inputs(self):
        self.reconcile("--build-only")
        relocated = self.root / "relocated-tools"
        relocated.mkdir()
        shutil.copy2(self.tools / "node", relocated / "node")
        self.env["PATH"] = f"{relocated}{os.pathsep}{self.env['PATH']}"
        self.change_js("relocated-node")
        self.reconcile("--build-only")
        self.assertEqual(len(self.commands("npm")), 6)
        self.assertEqual(len(self.prebuild_commands()), 2)
        shutil.copy2(self.tools / "pod", relocated / "pod")
        self.change_js("relocated-pod")
        self.reconcile("--build-only")
        self.assertEqual(len(self.commands("npm")), 6)
        self.assertEqual(len(self.prebuild_commands()), 3)
        self.change_js("different-xcode")
        self.reconcile("--build-only", "--developer-dir", str(self.root / "OtherXcode/Developer"))
        self.assertEqual(len(self.commands("npm")), 6)
        self.assertEqual(len(self.prebuild_commands()), 4)

    def test_failed_build_preserves_partial_cache_and_appends_retry_logs(self):
        self.configure(build_exit=65, artifact_on_failure=True)
        self.invoke()
        cache = self.state / "build-cache"
        first_inputs = json.loads((cache / "inputs.json").read_text())
        product = cache / "derived/Build/Products/Release-iphoneos/smalltalk.app"
        self.assertTrue((product / "Info.plist").exists())
        sentinel = cache / "derived/partial-cache-sentinel"
        sentinel.write_text("partial build cache")
        self.invoke()
        self.assertEqual(json.loads((cache / "inputs.json").read_text()), first_inputs)
        self.assertEqual(sentinel.read_text(), "partial build cache")
        self.assertFalse(self.saved_state().get("pending"))
        self.assertEqual(self.device_commands("install"), [])
        self.assertEqual(self.device_commands("launch"), [])
        artifact = self.state / "artifacts" / self.initial_commit
        self.assertFalse((artifact / "success.json").exists())
        self.assertFalse((artifact / "smalltalk.app").exists())
        self.assertEqual((artifact / "build.log").read_text().count("fixture build output"), 2)
        self.configure(build_exit=0)
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("npm")), 3)
        self.assertEqual(len(self.prebuild_commands()), 1)
        self.assertEqual(len(self.commands("pod")), 1)
        self.assertEqual(len(self.commands("xcodebuild")), 3)
        self.assertEqual((artifact / "build.log").read_text().count("fixture build output"), 3)

    def test_pending_snapshot_is_immutable_across_failed_and_successful_new_builds(self):
        self.configure(install_exit=1)
        self.invoke()
        first_app = Path(self.saved_state()["pending"]["app"])
        first_bytes = (first_app / "smalltalk").read_bytes()
        self.assertEqual(first_app.parent, self.state / "artifacts" / self.initial_commit)
        next_commit = self.change_js("snapshot-successor")
        self.configure(build_exit=65, artifact_on_failure=True)
        self.invoke("--build-only")
        # The superseded pending artifact is dropped; its snapshot stays intact.
        self.assertFalse(self.saved_state().get("pending"))
        self.assertEqual((first_app / "smalltalk").read_bytes(), first_bytes)
        self.assertEqual(len(self.device_commands("install")), 1)
        self.assertFalse((self.state / "artifacts" / next_commit / "success.json").exists())
        self.configure(build_exit=0)
        self.reconcile("--build-only")
        second_app = Path(self.saved_state()["pending"]["app"])
        self.assertEqual(second_app.parent, self.state / "artifacts" / next_commit)
        self.assertNotEqual(first_app, second_app)
        self.assertNotEqual((second_app / "smalltalk").read_bytes(), first_bytes)
        self.assertEqual((first_app / "smalltalk").read_bytes(), first_bytes)
        shared_product = self.state / "build-cache/derived/Build/Products/Release-iphoneos/smalltalk.app/smalltalk"
        self.assertEqual(shared_product.read_bytes(), (second_app / "smalltalk").read_bytes())
        self.configure(install_exit=0)
        self.reconcile()
        self.assert_installed(next_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 3)
        self.assertIn(str(second_app), self.device_commands("install")[-1]["args"])
        self.assertEqual((first_app / "smalltalk").read_bytes(), first_bytes)

    def test_native_plist_product_and_receipt_identify_exact_commit(self):
        for iteration in range(2):
            commit = self.initial_commit if iteration == 0 else self.change_js("identity-successor")
            with self.subTest(commit=commit):
                self.reconcile("--build-only")
                build_version = self.git("rev-list", "--count", commit)
                receipt = json.loads((self.state / "artifacts" / commit / "success.json").read_text())
                self.assertEqual(receipt["commit"], commit)
                self.assertEqual(receipt["build_version"], build_version)
                self.assertEqual(self.saved_state()["pending"]["build_version"], build_version)
                native = self.state / "checkout/apps/ios/ios/smalltalk/Info.plist"
                product = Path(receipt["app"]) / "Info.plist"
                for path in (native, product):
                    with path.open("rb") as info:
                        bundle = plistlib.load(info)
                    self.assertEqual(bundle["CFBundleVersion"], build_version)
                    self.assertEqual(bundle["StBuildCommit"], commit)
        self.assertEqual(len(self.prebuild_commands()), 1)

    def test_stale_product_source_identity_never_becomes_pending_or_installed(self):
        for control in ({"built_commit": "0" * 40, "built_version": None},
                        {"built_commit": None, "built_version": "0"}):
            with self.subTest(control=control):
                self.configure(**control)
                self.invoke()
                self.assertEqual(self.outcomes()[-1], "build_failed")
                self.assertFalse(self.saved_state().get("pending"))
                self.assertFalse(self.saved_state().get("installed"))
                self.assertEqual(self.commands("codesign"), [])
                self.assertEqual(self.device_commands("install"), [])
                self.assertEqual(self.device_commands("launch"), [])
                artifact = self.state / "artifacts" / self.initial_commit
                self.assertFalse((artifact / "success.json").exists())
                self.assertFalse((artifact / "smalltalk.app").exists())
                self.assertIn("Built app has stale source identity", (artifact / "build.log").read_text())

    def test_stale_js_bundle_stamp_never_becomes_pending_or_installed(self):
        self.configure(bundled_build="9f3740eddbde")
        self.invoke()
        self.assertEqual(self.outcomes()[-1], "build_failed")
        self.assertFalse(self.saved_state().get("pending"))
        self.assertFalse(self.saved_state().get("installed"))
        self.assertEqual(self.commands("codesign"), [])
        self.assertEqual(self.device_commands("install"), [])
        self.assertEqual(self.device_commands("launch"), [])
        artifact = self.state / "artifacts" / self.initial_commit
        self.assertFalse((artifact / "success.json").exists())
        self.assertFalse((artifact / "smalltalk.app").exists())
        self.assertIn("Built app embeds a stale build stamp", (artifact / "build.log").read_text())
        # The failed build must not record the inlined stamp, so the next
        # attempt resets the transform cache again instead of trusting it.
        inputs = json.loads((self.state / "build-cache" / "inputs.json").read_text())
        self.assertNotIn("js_env", inputs)

    def test_build_only_environment_holds_phone_and_resumes_without_rebuilding(self):
        self.env["ST_IOS_BUILD_ONLY"] = "1"
        self.reconcile()
        self.reconcile()
        self.assertEqual(self.outcomes()[-1], "install_held")
        self.assertEqual(self.saved_state()["pending"]["commit"], self.initial_commit)
        self.assertFalse(self.saved_state()["pending"]["installed"])
        self.assertEqual(self.commands("xcrun"), [])
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.env["ST_IOS_BUILD_ONLY"] = "0"
        self.reconcile()
        self.assert_installed(self.initial_commit)
        self.assertEqual(len(self.commands("xcodebuild")), 1)
        self.assertEqual(len(self.device_commands("install")), 1)
        self.assertEqual(len(self.device_commands("launch")), 1)


if __name__ == "__main__":
    unittest.main()

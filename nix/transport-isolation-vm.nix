# Runs three retained st2 controls and four current-runtime scope controls in a VM with a real
# systemd user manager for a lingering user, as on a fleet host. The isolation-vm CI job builds
# this test's driver and runs it outside the Nix sandbox with its prebuilt nextest archive:
#
#   ST_ISOLATION_ARCHIVE    workspace archive containing the retained st2 controls and st-runtime tests
#   ST_ISOLATION_WORKSPACE  the checkout the archive was built in; the test binary's compiled-in
#                           st2 path lies below it, so the archive is extracted at the same path.
#                           nextest also requires each selected package's working directory.
#   ST_ISOLATION_TIMINGS    JSON file that receives the boot and test durations
#
# The VM compiles nothing. KVM is required; there is no emulation fallback.
{ pkgs, pty }:
let
  user = "tester";
  uid = 1000;
  controls = [
    {
      package = "st2";
      binary = "integration";
      kind = "test";
      name = "transport_isolation::exec_task_survives_transport_cgroup_cascade";
    }
    {
      package = "st2";
      binary = "integration";
      kind = "test";
      name = "transport_isolation::pty_task_survives_transport_cgroup_cascade";
    }
    {
      package = "st2";
      binary = "integration";
      kind = "test";
      name = "nomad_survival::managed_agent_color_contract_crosses_systemd_scope";
    }
    {
      package = "st-runtime";
      binary = "scope_cascade";
      kind = "test";
      name = "exec_survives_parent_service_sigkill_with_the_same_incarnation_and_stops";
    }
    {
      package = "st-runtime";
      binary = "scope_cascade";
      kind = "test";
      name = "pty_survives_parent_service_sigkill_with_the_same_incarnation_and_stops";
    }
    {
      package = "st-runtime";
      binary = "scope_cascade";
      kind = "test";
      name = "literal_environment_is_preserved_across_a_work_scope";
    }
    {
      package = "st-runtime";
      binary = "st_runtime";
      kind = "lib";
      name = "isolate::retirement::native_tests::a_bound_scope_retires_escaped_descendants_and_preserves_a_neighbor";
    }
  ];
  controlFilter = pkgs.lib.concatStringsSep " or " (
    map (
      control:
      "(package(=${control.package}) and binary(=${control.binary}) and kind(${control.kind}) and test(=${control.name}))"
    ) controls
  );
  junitConfig = pkgs.writeText "isolation-nextest.toml" ''
    [profile.default.junit]
    path = "/tmp/isolation-junit.xml"
  '';
  validateControls = pkgs.writeText "validate-isolation-controls.py" ''
    import json
    import sys
    import xml.etree.ElementTree as ET
    from pathlib import Path

    expected = {tuple(control[key] for key in ("package", "binary", "kind", "name"))
                for control in json.loads('${builtins.toJSON controls}')}
    def unique_object(pairs):
        result = dict(pairs)
        assert len(result) == len(pairs), "duplicate inventory object key"
        return result

    inventory = json.loads(Path(sys.argv[2]).read_text(), object_pairs_hook=unique_object)
    selected = {}
    selected_binaries = {identity[:3] for identity in expected}
    for binary_id, suite in inventory["rust-suites"].items():
        binary = tuple(suite[key] for key in ("package-name", "binary-name", "kind"))
        if binary in selected_binaries:
            assert suite["status"] == "listed", "a selected binary could not be listed"
        for name, case in suite.get("testcases", {}).items():
            if case["filter-match"]["status"] != "matches":
                continue
            assert suite["status"] == "listed"
            identity = binary + (name,)
            assert identity not in selected, "duplicate selected identity"
            assert binary_id == suite["binary-id"]
            selected[identity] = (binary_id, name)
    assert set(selected) == expected, f"selected inventory differs: {set(selected) ^ expected}"
    report = Path("/tmp/isolation-junit.xml")
    if sys.argv[1] == "inventory":
        assert not report.exists(), "the result report must be fresh"
        print(f"Validated exact {len(expected)} authenticated-archive control identities")
    else:
        assert sys.argv[1] == "results"
        root = ET.parse(report).getroot()
        assert root.tag == "testsuites"
        assert int(root.attrib["tests"]) == len(expected)
        assert int(root.attrib["failures"]) == 0 and int(root.attrib["errors"]) == 0
        actual = set()
        suites = set()
        for suite in root:
            assert suite.tag == "testsuite"
            binary_id = suite.attrib["name"]
            assert binary_id not in suites, "duplicate result suite"
            suites.add(binary_id)
            assert all(int(suite.attrib.get(key, "0")) == 0
                       for key in ("disabled", "skipped", "failures", "errors"))
            assert int(suite.attrib["tests"]) == len(suite), "incomplete result suite"
            for case in suite:
                assert case.tag == "testcase"
                assert case.attrib["classname"] == binary_id
                identity = (binary_id, case.attrib["name"])
                assert identity not in actual, "duplicate final result"
                assert all(child.tag in ("system-out", "system-err") for child in case), "non-passing or retried result"
                actual.add(identity)
        assert suites == {binary_id for binary_id, name in selected.values()}, "missing or extra result suite"
        assert actual == set(selected.values()), "missing or extra final result"
        print(f"Validated exact {len(expected)} distinct controls passed without retries")
  '';
  runTests = pkgs.writeShellScript "run-transport-isolation" ''
    set -euo pipefail
    unset ST2_ALLOW_ISOLATION_SKIP ST2_ALLOW_PTY_SKIP
    export CI_TEST_EXTRACTED="$1" CI_TEST_BUILD_WORKSPACE="$2" CI_TEST_WORKSPACE="$1"
    export CI_TEST_TARGET_TMPDIR=/tmp/ci-test-scratch
    mkdir -p "$CI_TEST_TARGET_TMPDIR"
    common=(--archive-file /tmp/isolation.tar.zst --workspace-remap "$1" --extract-to "$1"
      --extract-overwrite --profile default --run-ignored all -E ${pkgs.lib.escapeShellArg controlFilter})
    cargo-nextest nextest list "''${common[@]}" --message-format json > /tmp/isolation-list.json
    ${pkgs.python3}/bin/python ${validateControls} inventory /tmp/isolation-list.json
    cargo-nextest nextest run "''${common[@]}" --retries 0 --no-tests=fail \
      --tool-config-file isolation:${junitConfig}
    ${pkgs.python3}/bin/python ${validateControls} results /tmp/isolation-list.json
  '';
in
pkgs.testers.runNixOSTest {
  name = "st2-transport-isolation";
  # QEMU fails instead of falling back to emulation without usable KVM.
  qemu.forceAccel = true;
  nodes.machine = {
    virtualisation.cores = 4;
    virtualisation.memorySize = 2048;
    # The workspace archive is copied here and expands to the complete prebuilt test inventory.
    # The former 4 GiB disk held the small legacy archive, but cannot hold both new inputs.
    virtualisation.diskSize = 16384;
    users.users.${user} = {
      isNormalUser = true;
      inherit uid;
      linger = true;
    };
    environment.systemPackages = [
      pty
      pkgs.cargo-nextest
    ];
  };
  testScript = ''
    import json
    import os
    import re
    import shlex
    import time

    archive = os.environ["ST_ISOLATION_ARCHIVE"]
    workspace = os.environ["ST_ISOLATION_WORKSPACE"]
    build_workspace = os.environ["ST_ISOLATION_BUILD_WORKSPACE"]

    started = time.monotonic()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("default.target", "${user}")
    boot_seconds = time.monotonic() - started
    virt = machine.succeed("systemd-detect-virt").strip()
    assert virt == "kvm", f"the VM runs under {virt}, not KVM"

    machine.copy_from_host(archive, "/tmp/isolation.tar.zst")
    # Archives carry built binaries, not package source directories. Provision the manifests
    # and working directories for the selected root st2 and crates/st-runtime packages.
    for manifest in ("Cargo.toml", "crates/st-runtime/Cargo.toml"):
        path = os.path.join(workspace, manifest)
        directory = shlex.quote(os.path.dirname(path))
        machine.succeed(f"mkdir -p {directory} && chown ${user} {directory}")
        machine.copy_from_host(path, path)
    # A transient service of the user's own manager: the tests start their scopes inside the
    # user's delegated cgroup subtree, as st2 does under a fleet seat.
    started = time.monotonic()
    output = machine.succeed(
        "su ${user} -s /bin/sh -c '"
        "XDG_RUNTIME_DIR=/run/user/${toString uid} systemd-run --user --wait --pipe --collect --quiet "
        "--setenv=PATH=/run/current-system/sw/bin "
        f"-- ${runTests} {shlex.quote(workspace)} {shlex.quote(build_workspace)}' 2>&1"
    )
    test_seconds = time.monotonic() - started
    print(output)
    assert re.search(r"\b7 tests run: 7 passed\b", output), "all seven explicitly selected systemd-scope cases must run and pass"

    with open(os.environ["ST_ISOLATION_TIMINGS"], "w") as timings:
        json.dump({"boot_seconds": round(boot_seconds, 1), "test_seconds": round(test_seconds, 1)}, timings)
  '';
}

# Runs st2's transport-isolation gate (tests/transport_isolation.rs) in a NixOS VM with a real
# systemd user manager for a lingering user, as on a fleet host. The isolation-vm CI job builds
# this test's driver and runs it outside the Nix sandbox with its prebuilt nextest archive:
#
#   ST_ISOLATION_ARCHIVE    nextest archive of st2's integration test binary and the st2 binary
#   ST_ISOLATION_WORKSPACE  the checkout the archive was built in; the test binary's compiled-in
#                           st2 path lies below it, so the archive is extracted at the same path.
#                           nextest also requires the workspace manifest there.
#   ST_ISOLATION_TIMINGS    JSON file that receives the boot and test durations
#
# The VM compiles nothing. KVM is required; there is no emulation fallback.
{ pkgs, pty }:
let
  user = "tester";
  uid = 1000;
  runTests = pkgs.writeShellScript "run-transport-isolation" ''
    set -euo pipefail
    exec cargo-nextest nextest run \
      --archive-file /tmp/isolation.tar.zst \
      --workspace-remap "$1" \
      --extract-to "$1" \
      --extract-overwrite \
      --no-tests=fail \
      -E 'package(=st2) and binary(=integration) and test(/^transport_isolation::/)'
  '';
in
pkgs.testers.runNixOSTest {
  name = "st2-transport-isolation";
  # QEMU fails instead of falling back to emulation without usable KVM.
  qemu.forceAccel = true;
  nodes.machine = {
    virtualisation.cores = 4;
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
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
    import time

    archive = os.environ["ST_ISOLATION_ARCHIVE"]
    workspace = os.environ["ST_ISOLATION_WORKSPACE"]

    started = time.monotonic()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("default.target", "${user}")
    boot_seconds = time.monotonic() - started
    virt = machine.succeed("systemd-detect-virt").strip()
    assert virt == "kvm", f"the VM runs under {virt}, not KVM"

    machine.copy_from_host(archive, "/tmp/isolation.tar.zst")
    machine.succeed(f"mkdir -p {workspace} && chown ${user} {workspace}")
    machine.copy_from_host(f"{workspace}/Cargo.toml", f"{workspace}/Cargo.toml")
    # A transient service of the user's own manager: the tests start their scopes inside the
    # user's delegated cgroup subtree, as st2 does under a fleet seat.
    started = time.monotonic()
    output = machine.succeed(
        "su ${user} -s /bin/sh -c '"
        "XDG_RUNTIME_DIR=/run/user/${toString uid} systemd-run --user --wait --pipe --collect --quiet "
        "--setenv=PATH=/run/current-system/sw/bin "
        f"-- ${runTests} {workspace}' 2>&1"
    )
    test_seconds = time.monotonic() - started
    print(output)
    assert re.search(r"\b2 tests run: 2 passed\b", output), "both cascade cases must run and pass"

    with open(os.environ["ST_ISOLATION_TIMINGS"], "w") as timings:
        json.dump({"boot_seconds": round(boot_seconds, 1), "test_seconds": round(test_seconds, 1)}, timings)
  '';
}

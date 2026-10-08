# Running onboarding on a real Linux VM

`scripts/onboarding-vm-test` uses the same runner as `scripts/onboarding-e2e`.
It supports baseline and setup, with Incus or an existing command transport.
Real-machine backends refuse CI environments. Nothing schedules them in CI.
An agent invokes them on a disposable machine with no real Smalltalk state for ada.

## Agent host preparation

Run the Linux passes on hetz2 under the separate onboarding-passes mission.
Operations owns Incus, QEMU/KVM, storage, network and agent account access through
that host's declarative NixOS configuration. The runner consumes that provisioned
host; it does not install host packages, initialize Incus, change groups, request
sudo or wait for a person to prepare the machine.

Before starting, the agent verifies its own session can use the local Incus
service and KVM, and that the configured default profile supplies a root disk
and network. These are read-only probes:

```sh
command -v incus
incus list --format=json
incus profile show default
incus storage list
incus network list
test -r /dev/kvm && test -w /dev/kvm
```

Image download, Ubuntu cloud-init and guest apt installation require network
access. The host needs capacity for one VM with two CPUs and 3 GiB RAM, plus its
root disk. Run one proof VM at a time and coordinate with other host users.
If provisioning or access is missing, route the exact failed probe to Operations;
do not replace declarative configuration with an interactive host change.

The seat also needs Python3 on the host, a checkout containing the runner, and
an immutable x86_64 Linux candidate archive with its expected SHA256. Copy the
candidate bytes to hetz2 through an authorized transport or download its pinned
URL; paths under another host's /var/tmp are not shared. Use the same archive
for both releases. Native Ubuntu22 requires an archive built for its libc
baseline; the host-linked Ubuntu24-only expert/cache proof archives do not
establish that compatibility. Neither provider credentials nor a host Smalltalk
store are needed for the setup/service/UI pass.

This repository's retained evidence remains container evidence until the
separate VM mission publishes actual Incus receipts. An agent starts these runs
outside CI; “manual” means deliberately invoked, not a request for a person.

## Managed disposable guest

From the repository containing the runner:

```sh
scripts/onboarding-vm-test baseline --backend incus --ubuntu 22.04 \
  --out /var/tmp/onboarding-real-baseline-22
scripts/onboarding-vm-test setup --backend incus --ubuntu 24.04 \
  --archive /tmp/candidate.tar.gz --sha256 HEX \
  --out /var/tmp/onboarding-real-setup-24
```

Use an adjacent candidate.tar.gz.sha256 or `--sha256 HEX`. Remove `--ubuntu` to
run both releases, sequentially. The backend launches
`images:ubuntu/RELEASE/cloud --vm` with two CPUs and 3 GiB RAM, waits for cloud-init,
installs python3/curl/CA certificates/dbus-user-session, creates ada without sudo
rights, and starts its user manager for the initial test. It copies the archive
through the exec transport; no host filesystems are mounted. It restarts the VM
and probes the user manager as root before logging in as ada, so a test login
cannot hide missing lingering. By default it deletes only its own unique guest.
Use `--keep` to retain it; the report records its name.

The container no-manager scenario is specifically a container fallback test.
Use setup or first-run for the real-VM service proof. A VM may allow lingering
through polkit while the container refuses it. In either case the report records
the actual outcome and verifies the corresponding post-reboot behavior.

## Existing fresh guest or SSH transport

For an existing disposable VM, use the external backend. The user must be ada,
with a fresh home and an available user manager. Install the same guest packages
as above. A snapshot taken before st is installed allows repeating the run.

```sh
incus snapshot create studio clean
export ST_VM_EXEC='incus exec studio --force-noninteractive -- runuser -u ada -- env HOME=/home/ada XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus'
export ST_VM_ROOT_EXEC='incus exec studio --force-noninteractive --'
export ST_VM_REBOOT='incus restart studio'
scripts/onboarding-vm-test setup --backend external --ubuntu 24.04 \
  --archive /tmp/candidate.tar.gz --out /var/tmp/onboarding-external
```

Use ada's actual UID in the runtime directory. ST_VM_EXEC must pass stdin through;
`ssh -o BatchMode=yes ada@ADDRESS` is another option; provision its key and host
trust before running. Values are split into argv without shell eval;
use a wrapper executable for complicated transports. ST_VM_ROOT_EXEC is required
for the reboot proof. ST_VM_REBOOT is optional and overrides root `systemctl reboot`.
The external backend creates and deletes nothing; restore or remove the named VM
yourself after reading the report. Do not target a workstation's regular home.

## Read and publish the report

Use a fresh OUT path for every attempt; the runner refuses an existing output
directory. Keep failing attempts and disclose any bounded rerun. Do not use
`--keep` for an unattended pass unless the mission explicitly needs a retained VM.

Each OUT/RELEASE-SCENARIO directory contains exact commands and output in
report.md, and machine-readable result.json. PASS means the assertion held. XFAIL
is a reproduced pinned baseline defect; XPASS requires reviewing that old
expectation. FAIL or XPASS returns exit 1. Candidate runs have no expected failures.
`--no-reboot` explicitly skips boot proof and must be disclosed with the evidence.

Native provider fixtures prove only plumbing. `--strict-release` adds final
archive/version checks. `--require-wrap-up` additionally requires a genuinely
completed mission; ordinary fixtures cannot supply that evidence. Details and
scenario limits are in [onboarding-e2e.md](onboarding-e2e.md).

Docker restart is a container restart. Incus boot, polkit, guest kernel/cgroups,
SSH login behavior and host provisioning remain untested until the separate VM
mission runs the passes and publishes their receipts. The macOS variant and
Tart backend are prepared in [onboarding-mac-runbook.md](onboarding-mac-runbook.md).
Native launchd, privacy, signing, app-bundle and reboot checks remain untested
until the Mac pass.

Publish every report.md and result.json as st documents, retaining the returned
immutable name@hash references. Run these as the VM mission seat with its own
ST_AGENT; the names below are examples for one run and must be unique per attempt:

```sh
st documents put /var/tmp/onboarding-real-setup-24/24.04-setup/report.md \
  --as doc/fleet/smalltalk/onboarding-passes/ATTEMPT/ubuntu24-report --json
st documents put /var/tmp/onboarding-real-setup-24/24.04-setup/result.json \
  --as doc/fleet/smalltalk/onboarding-passes/ATTEMPT/ubuntu24-receipt --json
```

Repeat for Ubuntu22. A completion summary pins the runner git revision, archive
SHA256 and BUILD.json source/binary hashes, all report references, counts, actual
backend and reboot outcome, and confirms each report's named Incus guest has
been removed. Disclose `--no-reboot`, supplied runtimes, provider fixtures or any
other departure from a native VM pass. A failed boot or cleanup remains a failure;
container receipts do not stand in for VM results.

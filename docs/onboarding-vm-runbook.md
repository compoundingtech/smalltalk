# Running onboarding on a real Linux VM by hand

`scripts/onboarding-vm-test` uses the same runner as `scripts/onboarding-e2e`.
It supports baseline and setup, with Incus or an existing command transport.
Real-machine backends refuse CI environments. Nothing schedules them in CI.
Run them manually on a disposable machine, with no real Smalltalk state for ada.

## Host preparation, performed by the person

Incus is not installed on the test host and the test account cannot access
`/dev/kvm`. Installing the VM host service and adding group membership requires
root. The rig does not perform this action or ask for a password. Consult the
[Incus installation instructions](https://linuxcontainers.org/incus/docs/main/installing/)
for packages appropriate to the host distribution. On Ubuntu with the packages
available, the person can run:

```sh
sudo apt-get install incus qemu-system-x86
sudo incus admin init --minimal
sudo adduser "$USER" incus-admin
sudo adduser "$USER" kvm
```

Initialize only a new Incus installation; keep an existing installation's network
and storage configuration. Log out and back in, then verify:

```sh
incus list
test -r /dev/kvm && test -w /dev/kvm
```

Incus's documentation describes [instance creation](https://linuxcontainers.org/incus/docs/main/howto/instances_create/)
and [snapshot creation](https://linuxcontainers.org/incus/docs/main/reference/manpages/incus/snapshot/create/).
The commands below follow those interfaces; no Incus guest has been run as part
of the current evidence. Host provisioning remains a person-owned action.

## Managed disposable guest

From the repository containing the runner:

```sh
scripts/onboarding-vm-test baseline --backend incus --ubuntu 22.04 \
  --out /var/tmp/onboarding-real-baseline-22
scripts/onboarding-vm-test setup --backend incus --ubuntu 24.04 \
  --archive /tmp/candidate.tar.gz --out /var/tmp/onboarding-real-setup-24
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

For a VM already created by hand, use the external backend. The user must be ada,
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
`ssh ada@ADDRESS` is another option. Values are split into argv without shell eval;
use a wrapper executable for complicated transports. ST_VM_ROOT_EXEC is required
for the reboot proof. ST_VM_REBOOT is optional and overrides root `systemctl reboot`.
The external backend creates and deletes nothing; restore or remove the named VM
yourself after reading the report. Do not target a workstation's regular home.

## Read the report

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
SSH login behavior and host provisioning remain untested until a person runs this
VM pass. macOS/launchd, permissions, signing, the app bundle and Tart belong to the
later fresh-mac-prep step; this Linux runner does not claim Mac coverage.

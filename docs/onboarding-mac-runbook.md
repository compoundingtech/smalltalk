# Running onboarding on a fresh Mac by hand

`scripts/onboarding-mac-test` is the macOS entry point to the shared onboarding rig.
It defaults to the Tart backend; `scripts/onboarding-vm-test --backend tart` also
works. Run it manually on an Apple Silicon Mac. It refuses CI, Linux hosts,
non-Darwin archives and missing SSH keys before creating a VM. This runner has
been tested with local fault fixtures on Linux; no native Mac pass is claimed.

## Prepare a reusable clean base

Install Tart using its [official quick start](https://tart.run/quick-start/).
The [Tart CLI source](https://github.com/cirruslabs/tart/tree/main/Sources/tart/Commands)
documents clone, set, run, ip, stop and delete. The runner uses two CPUs, 4096 MiB,
no shared host directories, and disables Tart's automatic pruning of other VMs.
It clones only the prepared local base, uses a unique name and deletes only its
own clone. `--keep` retains the running clone for inspection.

On the host, create a dedicated test SSH key and a base VM:

```sh
ssh-keygen -t ed25519 -f ~/.ssh/st-onboarding-test
# Unlock a passphrase-protected key in your host SSH agent before running.
ssh-add ~/.ssh/st-onboarding-test

tart create --from-ipsw=latest studio-clean
tart set studio-clean --cpu 2 --memory 4096
tart run studio-clean
```

Finish macOS installation by hand in the VM window. Create the invented user
`ada` and machine `studio`; use a disposable password chosen by the person.
Configure automatic GUI login as ada, enable Remote Login for ada, and keep its
login session unlocked. Install Python 3.11 or newer for the test instrumentation
and archive installer. Make Python available in ada's fresh login shell. Leave
Smalltalk, provider CLIs, gh, Rust and other developer tools absent. A base image
with a preinstalled provider/toolchain will fail the clean-tool-path assertion.
Do not grant Smalltalk privacy permissions in the clean base.

Copy only the **public** SSH key into `/Users/ada/.ssh/authorized_keys` inside the
VM, set the directory mode to700 and file mode to600. Never copy the host private
key into the guest. Check access from the host:

```sh
ssh -i ~/.ssh/st-onboarding-test -o BatchMode=yes ada@"$(tart ip studio-clean)" \
  'id; command -v python3; stat -f %Su /dev/console; launchctl print "gui/$(id -u)" >/dev/null'
tart stop studio-clean
```

The console owner must be ada. SSH alone does not create the GUI domain used by
`st service install`; the runner waits for that actual domain. The person prepares
these OS settings and any password/keychain unlocks directly. The runner disables
SSH password and keyboard-interactive authentication and never invokes sudo.

## Run the candidate

Use an `aarch64-apple-darwin` release/candidate archive containing `bin/st3`,
`bin/st`, `bin/pty`, `install.sh`, `install-macos.py` and BUILD.json. Pin the archive
with an adjacent .sha256 file or an explicit `--sha256 HEX`. The candidate must
contain the composed setup/TUI/stui-removal work and the current install-story
Mac caller recovery fix. The native Linux proof archive is not a Mac candidate.
The recovery fix was source d438e033cba3f3efd2138d6097edd5380aa1d28c when this
runbook was prepared; use its equivalent if the candidate has been rebased.

```sh
scripts/onboarding-mac-test --scenario first-run \
  --archive /tmp/smalltalk-aarch64-apple-darwin.tar.gz \
  --tart-image studio-clean --ssh-key ~/.ssh/st-onboarding-test \
  --strict-release --keep --out /tmp/onboarding-mac-first-run

# A second fresh clone tests setup flags and config merge rather than first-run answers.
scripts/onboarding-mac-test --scenario setup \
  --archive /tmp/smalltalk-aarch64-apple-darwin.tar.gz \
  --tart-image studio-clean --ssh-key ~/.ssh/st-onboarding-test \
  --strict-release --out /tmp/onboarding-mac-setup
```

`--scenario no-harness` is another flags-based run. All three cases require a
clean base without providers; they verify the explicit no-harness explanation and
absence of the built-in expert/mission. Provider transport and model/content
completion remain separate tests. Multiple `--scenario` options create fresh
clones sequentially. No task is scheduled in CI.

Each case checks the copied archive hash, installs the extracted archive as ada,
verifies `~/Applications/SmallTalk.app` with strict deep codesign, checks that st
resolves to `Contents/MacOS/st3`, records its bundle identifier and designated
requirement, and requires stui to be absent. It then checks setup or the three
first-run questions, config, login PATH, real launchd status, ordinary doctor,
permissions guidance, Home/Now and Ctrl+Q in a real guest PTY. A repeated archive
install must preserve the app identity and leave the daemon running.

The default final check uses Tart stop/run to power-cycle the real guest, requires
a changed boot-session UUID, waits for ada's GUI auto-login and checks that launchd
starts the daemon again. This proves service startup **after GUI login**. It does
not claim daemon availability before a user logs in. `--no-reboot` explicitly skips
this evidence and is recorded in result.json.

## Permissions and signing, checked by the person

The runner records `st service permissions` and requires guidance for the actual
app executable, Full Disk Access, Developer Tools and service restart. Successful
guidance is not proof that macOS granted access. The runner opens a Tart VM window and `--keep` leaves it available for these checks.
Use that retained window; do not run a second `tart run` for an already-running VM.
In ada's guest Terminal:

```sh
~/.local/bin/st service permissions --open
```

Follow the printed System Settings steps for the actual installed executable.
Approve Full Disk Access and Developer Tools by hand. Then run:

```sh
~/.local/bin/st service restart
~/.local/bin/st service status
~/.local/bin/st doctor
```

Record the approvals, screenshots if useful, and a concrete service-owned access
test for the intended workspace. A successful doctor or a Terminal-owned file
read alone does not prove service-owned privacy access. The runner neither edits
TCC databases nor grants approvals, and reports this limit explicitly.

The default installer uses ad-hoc signing. For a persistent identity, provision a
dedicated signing identity in the disposable guest keychain by hand and unlock it
before the run. Pass explicit `--macos-signing-identity` and optionally
`--macos-signing-team`. The installer receives these as guest environment values;
a team without an identity is rejected. The runner verifies the expected team if
provided. Do not copy a production private signing key into the VM. Refer to
[macOS installation and signing](st3/macos-installation.md) for the product contract.

A same-payload rerun checks identity preservation, not permission persistence
across changed builds. To test that, install a second pinned Darwin archive in the
retained guest with the same explicit identity/team, compare designated requirements,
restart the service, and repeat the actual service-owned access test. Record both
archive hashes and approval changes. This requires native Mac execution.

If installation reports retained prior files or locks, follow its recovery guidance
and inspect the current app/links and saved helper job before removing locks.
A late archive caller must not overwrite a newer installation with an old backup.

## Retain evidence and clean up

Each OUT/macos-SCENARIO directory contains report.md, result.json, tart-run.log and
the isolated SSH known_hosts file. The JSON records every guest command/output,
host Tart lifecycle command, archive/build pins, exact VM name, assertions and
limits. Inspect native results before publishing them: signing identities, guest
IP addresses and OS account data belong in private evidence, not the public repo.
No password, API key or signing private key belongs in any receipt.

The default removes its unique clone after a pass or failure. If `--keep` was used,
use the **machine name from result.json**, not the clean base name:

```sh
tart stop st-onboarding-EXAMPLE
tart delete st-onboarding-EXAMPLE
```

No Tart guest, macOS installer, real codesign/keychain operation, launchd, TCC,
GUI login or native PTY execution was tested on the Linux preparation host.
The local fixture checks prove runner refusal, ownership/cleanup and transport
boundaries. The native first-run, signing, privacy and reboot assertions remain
for the person-run fresh-Mac pass.

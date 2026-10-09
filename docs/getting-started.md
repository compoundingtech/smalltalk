# Get started with Smalltalk

Download Smalltalk and run `st`. The first run sets up your machine and opens a
conversation with the built-in expert, when a coding harness is available. The expert
helps you give agents a mission and follow its progress in the terminal interface.

Start with one machine. Each person currently runs their own fleet; a phone, another
machine and GitHub integration can wait until you need them.

## 1. Download and open st

Run this in your terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/compoundingtech/smalltalk/main/install.sh | sh
```

The installer detects your platform, downloads the latest release and its SHA256
sidecar, checks the archive, installs `st3`, its `st` link and `pty` in `~/.local/bin`,
and opens `st`. The checksum comes from the same release endpoint as the archive;
it checks the downloaded bytes, rather than supplying an independent signature.

Linux x86_64 needs glibc 2.35+ (Ubuntu 22.04+), `curl`, `tar` and `sha256sum` or
`shasum`. Apple Silicon macOS needs macOS 15+, those download tools and Python 3.
If a required tool is missing, install it with your operating system's package
manager before downloading Smalltalk. Neither Rust, Nix nor the GitHub CLI is needed.

For an existing installation, read [upgrading st](upgrading-st.md) and the selected
release's Upgrade impact before replacing binaries or restarting a populated store.
To download without opening the interface, use the same installer with `--no-run`.
`--tag` chooses a release and `--bin-dir` changes the command destination; see
[binary releases](st3/binary-releases.md).

On macOS, the installer puts `st3` in `~/Applications/SmallTalk.app` and links the
command to that stable app path. Configure an optional persistent signing identity
before installation using [macOS installation and signing](st3/macos-installation.md).
The default is ad-hoc signing, which can require renewed OS approvals after an update.
Native installer and Darwin archive tests run in CI; clean-machine real-app, launchd
and TCC permission setup remain a separate manual rehearsal.

## 2. Answer the first-run screen

In a terminal, `st setup` (and the first plain `st`) shows one screen instead of asking
questions one at a time. It says in a few lines what Smalltalk is and that your first
agent runs on your own Claude or Codex account and uses its usage. Then it lists
everything to choose:

- **Your name and this machine's name.** `ada` and `studio` are invented examples; use
  your own. Names use lowercase letters, digits, hyphens and underscores, and the
  machine name `local` is reserved. Setup merges them into `~/.config/st3/config.toml`
  without replacing unrelated settings.
- **Start st when I log in**, ticked. This installs a user service (no administrator
  rights) and the screen says where it goes and how to remove it. On Linux, **Keep
  running after I log out** appears when lingering is off and can be turned on for you.
  If a user service manager is unavailable, setup explains the detached-daemon fallback;
  it does not survive a reboot. On macOS, follow the permission instructions in
  [daemon details](#daemon-details).
- **Every supported agent**: Claude Code, Codex, OpenCode, Pi and Omp. Those found on the
  daemon's login PATH are ticked and you can untick them. Each row keeps four facts
  apart: whether its program was found, whether it is selected, whether st's integration
  is ready, and whether you are signed in (or that setup cannot tell). Each row says what
  setup does for it and the exact local paths it touches. An agent that is not installed
  says how to get it; install it, then press `r` to look again.
- **Let Codex work without asking**, only when Codex is found, with the two levels it can
  offer (workspace only, or full access). The choice is saved as `codex_access` in the
  config (`ask`, `workspace` or `full`).

One consent line sits above the two buttons: *Set up st for the selected agents, adding
its local integrations and required workspace configuration?* **Set up** does it for
every ticked agent at once; **Skip for now** sets up st itself and no agent. Signing in,
trusting a folder and choosing a permission mode are never part of that consent. Each
agent ends **ready**, **needs you to sign in**, or **failed**, with the whole error on
screen, and one agent's failure never undoes another's setup. If an agent has not
finished its own first launch (Claude's theme and sign-in, a Codex or OpenCode login),
setup hands you the terminal for it and carries on when you leave it. **Retry** runs only
the rows that did not finish. The first agent that is ready starts the Assistant while the
screen is still up.

Pipes, `--yes` and anything that is not a terminal keep the line-mode setup: flags
supply the answers and nothing is asked. `st setup --help` lists them.

An installed program does not prove that its account is signed in. With no supported
agent, setup starts no onboarding run and still opens the interface. After installing
one, make the command available in this shell and run:

```sh
export PATH="$HOME/.local/bin:$PATH"
st setup
```

For Claude, setup offers the user-owned Smalltalk message channel without needing
administrator rights. A machine approval policy is optional: without it, seats use
the development-channel flag and st handles its admission dialog. Provider or
organization restrictions can still block the channel. Smalltalk never asks for or
types a password; optional machine policy, GitHub login and `gh` do not block setup.

Your agents run without permission prompts inside their own workspaces. Choose the
projects and tasks you give them accordingly.

## 3. Work with the expert

When a usable harness is available, first-run setup creates `agent/st/expert` and
starts `mission/st/onboarding`. When setup publishes the expert during a plain `st`
launch, the interface opens that conversation. You can also find it under Agents.
Follow its guidance for your first mission; the expert stays available afterward.

Home shows questions, decisions and failures that need you. Agents shows conversations
and transcripts; Missions shows work and results. **Ctrl+K** opens the palette and
**Ctrl+Q** quits the interface while agents keep working.

After quitting, make the installed commands available in this shell and reopen st:

```sh
export PATH="$HOME/.local/bin:$PATH"
st
```

If setup reported that the login PATH lacks `~/.local/bin`, add that export to your
shell's startup file so new terminals can find `st` too. The installer can open st
using its full path even before that PATH change.

You can also send the expert a question from the shell. This example assumes you
chose `ada`; replace `person/ada` with your configured person:

```sh
st conversations send agent/st/expert --from person/ada \
  --subject 'Getting started' --body 'Help me make my first mission.'
st
```

Ordinary setup consults onboarding run history across the fleet and does not start
another first run or resurrect a stopped expert. To begin again deliberately, finish
or cancel the active onboarding run, then use `st setup --onboarding`. That command
can restore a stopped expert and starts a distinct onboarding run. It does not cancel
an existing run for you. Use `st setup --help` for scripted answers and other options.

## 4. Check the installation

```sh
st --version
st doctor
st service status
```

`doctor` should show a reachable daemon and `pty`. A machine with no fleet configured
is healthy. If an older daemon was already running, compare `st --version --json`
with its `machine_version` in `st doctor --json` before continuing. See
[when something is wrong](when-something-is-wrong.md) for failed checks, provider
login and channel-delivery problems.

## Reference

### With Nix and Home Manager

Nix users can install a pinned release source with `nix profile install` instead of
using the archive. Choose a source after reading its Upgrade impact; see
[binary releases](st3/binary-releases.md) and [development](development.md#build-from-source)
for the package and source-build commands. Nix supplies build dependencies and the
pinned PTY runtime. Do not mix an archive destination with a Nix-owned command path.

The [Home Manager module](home-manager.md) installs the tools, writes person
configuration and starts the user daemon on Linux or macOS. If it owns your daemon,
configure the person there and let the module manage service definitions. Harness
login, lingering, OS permissions and fleet joining remain host setup. Nix profile
installs use store paths; source and release installers own the macOS app bundle.

### Daemon details

The background service is a systemd user unit on Linux and a launchd agent on macOS.
To inspect it, use `st service status`. On Linux, lingering allows the service to run
after logout and before login after a reboot. Setup prints the needed command if it
cannot enable it. On macOS, run:

```sh
st service permissions
```

Follow the instructions for the OS permissions you must grant, including Full Disk
Access and Developer Tools. Smalltalk does not automate those approval prompts.

For an upgrade, `st service install` refreshes definitions and restarts services;
`st service restart` suffices when definitions already point at the intended binaries.
Coordinate that restart with active work and any continuous health checks. Replacing
archive files alone does not restart running services.

State lives in `~/.local/state/st3`. The local API uses a Unix socket. On Linux it
normally lives in `XDG_RUNTIME_DIR`, with `STATE/run/st3.sock` linking to it; on macOS
it lives in the state directory. An explicit `--endpoint` or `ST3_ENDPOINT` takes
precedence. Commands wait up to 30 seconds for a restarting daemon, then exit with
status 5; `--daemon-wait` and `ST3_DAEMON_WAIT` change that wait. Restarting the daemon
leaves durable seats running so it can adopt them again. For foreground operation,
use `st up`.

The [daemon troubleshooting guide](when-something-is-wrong.md),
[command recorder](st3/command-recorder.md) and [compatibility policy](st3/compatibility.md)
cover socket paths, recorded commands and recovery details.

## Next

- [Missions in practice](missions-in-practice.md): define work, revise it, and answer person-only decisions.
- [Talking to agents](talking-to-agents.md): UI, phone, CLI, attachments, and requests.
- [Two machines](two-machines.md): create a fleet, invite another machine and check replication.
- [Terminal UI and iOS](stui-and-ios.md): follow work and pair the phone app.
- [Runnable examples](../examples/st3/README.md): seats, other harnesses, schedules and gates.
- [Seat lifecycle](seat-lifecycle.md): restart, suspend/resume and stopped seats.
- [GitHub integration](github-integration.md): optional repository intake and review.

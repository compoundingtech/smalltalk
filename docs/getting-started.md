# Get started with Smalltalk

Smalltalk turns your coding agents into a team that doesn't need babysitting. Describe the work once as a mission, with its goals, its constraints and the checks that prove it's done right, not just done. Agents on your own machines carry it through, survive restarts, and bring you only the decisions that are yours to make. You plan and decide; your agents do the rest, all day and overnight.

For now, each person runs their own fleet. Start with one machine; [add your second machine](two-machines.md) when you need it.

## 1. Install

Choose one route. If you already use Nix, go straight to [With Nix](#with-nix).

### Prebuilt release

Use a terminal running Bash or Zsh. Choose the script for your machine.

#### Linux x86_64

The prebuilt tools need glibc 2.35+ (Ubuntu 22.04+) and `curl` and `tar`. On a fresh Ubuntu machine, install the prerequisites:

```sh
sudo apt-get update
sudo apt-get install -y curl ca-certificates git
```

Download, verify, and install the latest release:

```sh
mkdir -p ~/smalltalk-install
cd ~/smalltalk-install
archive=smalltalk-x86_64-unknown-linux-gnu.tar.gz
release=https://github.com/compoundingtech/smalltalk/releases/latest/download
curl -fLO "$release/$archive"
curl -fLO "$release/$archive.sha256"
sha256sum -c "$archive.sha256"
tar -xzf "$archive"
"./${archive%.tar.gz}/install.sh" --bin-dir "$HOME/.local/bin"
export PATH="$HOME/.local/bin:$PATH"
```

#### macOS Apple Silicon

**Not yet verified on a fresh Mac:** the prebuilt tools need macOS 15+, `curl`, `tar`, and Python 3. The source installer (`scripts/install`) and extracted release installer put `st3` and `stui` inside `~/Applications/SmallTalk.app`, with links in your bin directory. macOS ties permissions, such as the microphone for stui's voice mode, to this stable app identity so grants can survive updates. Without a signing identity, the app is ad-hoc signed and macOS may ask again after each update. An optional Developer ID setting avoids that repeated approval; configure it **before installing**, using [macOS signing](st3/macos-installation.md).

The optional voice helper, `StListen.app`, needs Xcode with the macOS 26 SDK to build. You can use st without it; voice mode says when the helper is missing.

Download, verify, and install the latest release:

```sh
mkdir -p ~/smalltalk-install
cd ~/smalltalk-install
archive=smalltalk-aarch64-apple-darwin.tar.gz
release=https://github.com/compoundingtech/smalltalk/releases/latest/download
curl -fLO "$release/$archive"
curl -fLO "$release/$archive.sha256"
shasum -a 256 -c "$archive.sha256"
tar -xzf "$archive"
"./${archive%.tar.gz}/install.sh" --bin-dir "$HOME/.local/bin"
export PATH="$HOME/.local/bin:$PATH"
```

Keep that PATH in new terminals:

```sh
case "$SHELL" in
  */zsh) printf '\nexport PATH="$HOME/.local/bin:$PATH"\n' >> ~/.zprofile ;;
  *) printf '\nexport PATH="$HOME/.local/bin:$PATH"\n' >> ~/.profile ;;
esac
```

### With Nix

Install the same tools from a pinned release source (this can take a while).
The command uses v0.3.16 as an example; choose a release after reading its Upgrade impact:

```sh
nix --extra-experimental-features 'nix-command flakes' profile install github:compoundingtech/smalltalk/v0.3.16
command -v st stui pty
```

Nix builds `st3`, its `st` alias, `stui`, `st3-migrate`, and `sekrets`, and supplies the pinned `pty` runtime and build dependencies. You do not need a separate Rust toolchain or PTY install. Use this **instead of** the archive route; the commands below are the same. Check that the paths above belong to your Nix profile, then continue with the daemon setup.

For a declarative setup, use the [Home Manager module](home-manager.md): it installs the tools, writes the person configuration, and starts the user daemon on Linux or macOS. If that module owns your daemon, configure its person there and skip the manual config/service-install block in step 3. Lingering on Linux, macOS permissions, harness login, and fleet joining remain host setup. Nix profile installs use store paths; the macOS app-bundle setup above belongs to the source/release installers.

Archive installation replaces files; it does not restart an existing daemon. For an existing
fleet, use [upgrading st](upgrading-st.md) before running the new build against populated state.
Keep the downloaded archive, checksum and `BUILD.json` as the installed-source record.

See [binary releases](st3/binary-releases.md) for pinned versions and upgrades, and [macOS installation](st3/macos-installation.md) for Python, signing, and permission setup.

The default, `st`, `st3`, and `small-talk` Nix package names all select this package.
The previous generation builds separately as `.#st2`; install it explicitly with
`nix profile install .#st2`. For a source install without Nix, see
[development](development.md#build-from-source).

## 2. Prepare your workspace

This walkthrough assumes you already installed and logged in to [Claude Code](https://code.claude.com/docs/en/setup) as the OS user who will run `st`. `~/st/garden` is just the test area for these examples; put it anywhere you like:

```sh
mkdir -p ~/st/garden
cd ~/st/garden
git init
```

Install Smalltalk's Claude message channel (it may ask for your administrator password):

```sh
st claude-channel install
st claude-channel status
```

Other harnesses work too; their [seat examples](../examples/st3/README.md#run-a-durable-agent) show the KDL to use.

## 3. Start the daemon

Choose a person identity. `ada` is an invented example; use the same identity on your own machines.

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/st3"
printf 'person = "person/ada"\n' > "${XDG_CONFIG_HOME:-$HOME/.config}/st3/config.toml"
st service install
st service status
st doctor
```

On Linux, keep the user service running after logout:

```sh
loginctl enable-linger "$USER"
```

On macOS, run the permission helper and follow its instructions:

```sh
st service permissions
```

`doctor` should report a reachable daemon and `pty`.

Compare `st --version --json` with the daemon's `machine_version` in `st doctor --json` if
an older daemon was already running. A machine with no fleet configured is healthy. Warnings about optional build tools, GitHub login, or Linux IO priority do not block this walkthrough. Fix any failed check before continuing; [daemon setup](#daemon-details) has the details.

## 4. Start an agent and attach

From your test workspace, start an agent and open its terminal in one command:

```sh
st agents new garden/explorer --workspace "$PWD" --attach
```

Finish any first-run login or workspace trust prompts in that terminal. Press **Ctrl+\\** to detach; the agent keeps running. Stop this example when you are done exploring:

```sh
st agents stop agent/garden/explorer --as person/ada
```

## 5. Declare a durable seat

A declared seat restarts with its declaration and can be given missions. Save its workspace and harness in a KDL file in your test area:

```sh
cd ~/st/garden
cat > worker.kdl <<EOF
version 2
agent "garden/worker" {
  workspace "$PWD"
  restart "always"
  harness "claude" {
    model "sonnet"
  }
}
EOF
st apply worker.kdl --as person/ada
st agents show agent/garden/worker
st terminals attach agent/garden/worker
```

The harness starts and waits for work. Finish any first-run prompts in its terminal, then press **Ctrl+\\** to detach; the seat keeps running.

## 6. Give it a mission

Create a short work brief and a finite mission. Goals describe the result; the brief supplies instructions.

```sh
cat > BRIEF.md <<'EOF'
Create garden-note.md with three sentences about an invented community garden.
Use only invented names. Keep the note in this workspace.
EOF
cat > first-mission.kdl <<'EOF'
version 2
mission "garden/first-note" state="ready" {
  goal "garden-note.md contains a three-sentence introduction to an invented community garden."
  constraint "Follow BRIEF.md in the workspace."
  step "write" timeout="20m" {
    assigned-to "agent/garden/worker"
    goal "garden-note.md exists and follows the brief."
  }
}
EOF
st apply first-mission.kdl --as person/ada
st missions start garden/first-note --id garden/first-note/one \
  --workspace "$PWD" --as person/ada
stui
```

The new step wakes the seat automatically. In `stui`, open **Missions** and select `garden/first-note`; open **Agents** to see the worker. Use the sidebar or **Ctrl+K** to find them. **Ctrl+Q** quits the UI and leaves the work running.

Back in the shell, inspect the run. Once it says `completed`, read the result:

```sh
st missions show mission-run/garden/first-note/one
cat garden-note.md
```

## 7. Send a message

```sh
st conversations send agent/garden/worker --from person/ada \
  --subject 'Hello' --body 'What did you finish in the first mission?'
stui
```

Open the worker under **Agents** to read its answer. Messages are conversation; put new work in a mission so its result is tracked.

When you are done experimenting:

```sh
st agents stop agent/garden/worker --as person/ada
```

## Daemon details

Run `st service install` again after upgrading the binaries. It updates the installed definitions
and restarts the services so they use the new executables.

On Linux the service is a systemd user unit. It needs a working user manager; enable lingering
(`loginctl enable-linger`) so seats keep running after you log out. On macOS it is a launchd
agent; run `st service permissions` once for the Full Disk Access and Developer Tools steps.

State lives in `~/.local/state/st3` and the local API is a Unix socket. Restarting the daemon
does not stop running seats; it adopts them. While it restarts, a command waits up to 30 seconds
for it (`--daemon-wait SECONDS` or `ST3_DAEMON_WAIT` changes that) and then exits with status 5.
Seat drivers wait as long as the restart takes, keep their notes out of the seat's terminal in
`~/.local/state/st3/driver-api-warnings.log`, and resume from the graph. To run the daemon in the
foreground instead, use `st up`.

On Linux, the daemon normally listens in `XDG_RUNTIME_DIR`; it also publishes
`STATE/run/st3.sock` as a link to that socket, so commands without the daemon's
runtime environment can reach it. On macOS the socket already lives at that state path.
An explicit `--endpoint` or `ST3_ENDPOINT` still takes precedence.

st records every `git` and `gh` call it starts, including its own, in
`~/.local/state/st3/recorder/commands.jsonl`, then runs the real program unchanged. A call by
absolute path is not recorded. The [command recorder](st3/command-recorder.md) describes the
log.

If the state directory has a long path, set `XDG_RUNTIME_DIR` to a shorter directory or pass
`--socket` and `--client-gateway-socket` to `st up` so both Unix socket paths fit the OS limit.
When `st up` receives a private `--state-dir` or `--socket` without an explicit
`--client-gateway-socket`, its paired gateway is placed beside that private socket (or
in the private state directory when no socket is specified). An existing live listener
at either socket path is never replaced; choose a different path instead.

## Next

- [Using stui and the iOS app](stui-and-ios.md): follow work, talk to agents, and install the phone app.
- [Two machines](two-machines.md): connect your machines and check graph replication.
- [Missions in practice](missions-in-practice.md): revise work, queue runs, and ask for human decisions.
- [Talking to agents](talking-to-agents.md): UI, phone, CLI, attachments, and structured requests.
- [Build and run the iOS app](ios-app.md): local simulator and iPhone builds, then gateway pairing.
- [Seat lifecycle](seat-lifecycle.md): restart, suspend/resume, native sessions, and stopped seats.
- [0.x compatibility](st3/compatibility.md): upgrade baselines, clients, harnesses and current platform proof.
- [Upgrading st](upgrading-st.md): install the same build everywhere and check recovery options.
- [GitHub integration](github-integration.md): repository intake, review, triage, and landing work.
- [When something is wrong](when-something-is-wrong.md): health, work, usage, and stop reasons.
- [Runnable examples](../examples/st3/README.md): other harnesses, schedules, gates, and parallel work.
- [KDL lifecycle](st3/kdl-lifecycle.md): the complete declaration and revision workflow.
- [Terminal client](../crates/stui/README.md): UI controls and remote clients.

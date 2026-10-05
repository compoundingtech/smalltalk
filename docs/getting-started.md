# Get started with Smalltalk

Smalltalk turns your coding agents into a team that doesn't need babysitting. Describe the work once as a mission, with its goals, its constraints and the checks that prove it's done right, not just done. Agents on your own machines carry it through, survive restarts, and bring you only the decisions that are yours to make. You plan and decide; your agents do the rest, all day and overnight.

For now, each person runs their own fleet. Start with one machine; [add your second machine](two-machines.md) when you need it.

## 1. Install

Choose one route. If you already use Nix, go straight to [With Nix](#with-nix).

### Prebuilt release

Use a terminal running Bash or Zsh. The prebuilt tools support Linux x86_64 with glibc 2.35+ (Ubuntu 22.04+), and Apple Silicon with macOS 15+; you need `curl` and `tar`, plus Python 3 on macOS. On a fresh Ubuntu machine, install the prerequisites:

```sh
sudo apt-get update
sudo apt-get install -y curl ca-certificates git
```

**On macOS — not yet verified on a fresh Mac:** Python 3 is required. The source installer (`scripts/install`) and extracted release installer put `st3` and `stui` inside `~/Applications/SmallTalk.app`, with links in your bin directory. macOS ties permissions, such as the microphone for stui's voice mode, to this stable app identity so grants can survive updates. Without a signing identity, the app is ad-hoc signed and macOS may ask again after each update. An optional Developer ID setting avoids that repeated approval; configure it **before installing**, using [macOS signing](st3/macos-installation.md).

The optional voice helper, `StListen.app`, needs Xcode with the macOS 26 SDK to build. You can use st without it; voice mode says when the helper is missing.

Download the latest release into a new directory:

```sh
mkdir -p ~/smalltalk-install
cd ~/smalltalk-install
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) archive=smalltalk-x86_64-unknown-linux-gnu.tar.gz ;;
  Darwin-arm64) archive=smalltalk-aarch64-apple-darwin.tar.gz ;;
  *) echo 'Use the Nix installation below for this platform'; exit 1 ;;
esac
release=https://github.com/compoundingtech/smalltalk/releases/latest/download
curl -fLO "$release/$archive"
curl -fLO "$release/$archive.sha256"
if command -v sha256sum >/dev/null; then
  sha256sum -c "$archive.sha256"
else
  shasum -a 256 -c "$archive.sha256"
fi
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

Install the same tools from source (this can take a while):

```sh
nix --extra-experimental-features 'nix-command flakes' profile install github:compoundingtech/smalltalk
command -v st stui pty
```

Nix builds `st3`, its `st` alias, `stui`, and `st3-migrate`, and supplies the pinned `pty` runtime and build dependencies. You do not need a separate Rust toolchain or PTY install. Use this **instead of** the archive route; the commands below are the same. Check that the paths above belong to your Nix profile, then continue with the daemon setup.

For a declarative setup, use the [Home Manager module](home-manager.md): it installs the tools, writes the person configuration, and starts the user daemon on Linux or macOS. If that module owns your daemon, configure its person there and skip the manual config/service-install block in step 3. Lingering on Linux, macOS permissions, harness login, and fleet joining remain host setup. Nix profile installs use store paths; the macOS app-bundle setup above belongs to the source/release installers.

See [binary releases](st3/binary-releases.md) for pinned versions and upgrades, and [macOS installation](st3/macos-installation.md) for Python, signing, and permission setup.

The default, `st`, `st3`, and `small-talk` Nix package names all select this package.
The previous generation builds separately as `.#st2`; install it explicitly with
`nix profile install .#st2`. For a source install without Nix, see
[development](development.md#build-from-source).

## 2. Prepare your workspace

A seat uses a separate coding tool and its account. This walkthrough assumes you already installed and logged in to [Claude Code](https://code.claude.com/docs/en/setup) as the OS user who will run `st`. Create the example workspace:

```sh
mkdir -p ~/st/garden
cd ~/st/garden
git init
```

Install Smalltalk's Claude message channel (it may ask for your administrator password). Finish any workspace trust prompt when you attach to the seat in step 4:

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

`doctor` should report a reachable daemon and `pty`. A machine with no fleet configured is healthy. Warnings about optional build tools, GitHub login, or Linux IO priority do not block this walkthrough. Fix any failed check before continuing; [daemon setup](#daemon-details) has the details.

## 4. Declare your first seat

A seat declares the agent's workspace and harness. Save this KDL file in the workspace you just trusted:

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
st agents apply worker.kdl --as person/ada
st agents show agent/garden/worker
st terminals attach agent/garden/worker
```

The harness starts and waits for work. Finish any first-run prompts in its terminal, then press **Ctrl+\\** to detach; the seat keeps running.

## 5. Give it a mission

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
st missions publish first-mission.kdl --as person/ada
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

## 6. Send a message

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

- [Two machines](two-machines.md): connect your machines and check graph replication.
- [Missions in practice](missions-in-practice.md): revise work, queue runs, and ask for human decisions.
- [Talking to agents](talking-to-agents.md): UI, phone, CLI, attachments, and structured requests.
- [Build and run the iOS app](ios-app.md): local simulator and iPhone builds, then gateway pairing.
- [Seat lifecycle](seat-lifecycle.md): restart, suspend/resume, native sessions, and stopped seats.
- [Upgrading st](upgrading-st.md): install the same build everywhere and check recovery options.
- [GitHub integration](github-integration.md): repository intake, review, triage, and landing work.
- [When something is wrong](when-something-is-wrong.md): health, work, usage, and stop reasons.
- [Runnable examples](../examples/st3/README.md): other harnesses, schedules, gates, and parallel work.
- [KDL lifecycle](st3/kdl-lifecycle.md): the complete declaration and revision workflow.
- [Terminal client](../crates/stui/README.md): UI controls and remote clients.

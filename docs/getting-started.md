# Get started with Small Talk

Small Talk (`st`) keeps coding agents running as durable **seats** and gives them work as **missions**. Its graph remembers work, messages, and decisions across agent and daemon restarts.

For now, each person runs their own fleet. Start with one machine; [add your second machine](two-machines.md) when you need it.

## 1. Install

Use a terminal running Bash or Zsh. The prebuilt tools support Linux x86_64 with glibc 2.35+ (Ubuntu 22.04+), and Apple Silicon with macOS 15+; you need `curl` and `tar`, plus Python 3 on macOS. On a fresh Ubuntu machine, install the prerequisites:

```sh
sudo apt-get update
sudo apt-get install -y curl ca-certificates git
```

**On macOS — not yet verified on a fresh Mac:** Python 3 is required. Both source and release installs put `st3` and `stui` inside `~/Applications/SmallTalk.app`, with links in your bin directory. macOS ties permissions, such as the microphone for stui's voice mode, to this stable app identity so grants can survive updates. Without a signing identity, the app is ad-hoc signed and macOS may ask again after each update. An optional Developer ID setting avoids that repeated approval; configure it **before installing**, using [macOS signing](st3/macos-installation.md).

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

**Already use Nix?** Instead of downloading the archive, install the same tools from source (this can take a while):

```sh
nix --extra-experimental-features 'nix-command flakes' profile install github:compoundingtech/smalltalk
```

See [binary releases](st3/binary-releases.md) for pinned versions and upgrades, and [macOS installation](st3/macos-installation.md) for Python, signing, and permission setup.

## 2. Log in to a coding harness

A seat uses a separate coding tool and its account. This walkthrough uses [Claude Code](https://code.claude.com/docs/en/setup); install and log in as the same OS user who will run `st`:

```sh
curl -fsSL https://claude.ai/install.sh | bash
export PATH="$HOME/.local/bin:$PATH"
mkdir -p ~/st/garden
cd ~/st/garden
git init
claude
```

Follow the login and workspace trust prompts. Once you reach Claude's prompt, type `/exit` to return to your shell. Install Small Talk's Claude message channel (it may ask for your administrator password):

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

`doctor` should report a reachable daemon and `pty`. A machine with no fleet configured is healthy. Warnings about optional build tools, GitHub login, or Linux IO priority do not block this walkthrough. Fix any failed check before continuing; [daemon setup](../README.md#run-the-daemon) has the details.

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

## Next

- [Two machines](two-machines.md): connect your machines and check graph replication.
- [Missions in practice](missions-in-practice.md): revise work, queue runs, and ask for human decisions.
- [Talking to agents](talking-to-agents.md): UI, phone, CLI, attachments, and structured requests.
- [Seat lifecycle](seat-lifecycle.md): restart, suspend/resume, native sessions, and stopped seats.
- [Upgrading st](upgrading-st.md): install the same build everywhere and check recovery options.
- [GitHub integration](github-integration.md): repository intake, review, triage, and landing work.
- [When something is wrong](when-something-is-wrong.md): health, work, usage, and stop reasons.
- [Runnable examples](../examples/st3/README.md): other harnesses, schedules, gates, and parallel work.
- [KDL lifecycle](st3/kdl-lifecycle.md): the complete declaration and revision workflow.
- [Terminal client](../crates/stui/README.md): UI controls and remote clients.

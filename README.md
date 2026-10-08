# Smalltalk

Download Smalltalk, then run `st`. It opens a terminal interface where you can talk to
your coding agents and follow their work. Give them a mission with goals, constraints
and checks; durable seats carry it through and bring you the decisions that need you.

```sh
curl -fsSL https://raw.githubusercontent.com/compoundingtech/smalltalk/main/install.sh | sh
```

The installer selects your platform, verifies the downloaded archive and opens `st`.
On the first run, choose your name and this machine's name, accept the background
service, and choose an installed coding harness. The built-in expert guides you
through your first mission and stays available afterward.

After quitting the interface with **Ctrl+Q**, open it again:

```sh
export PATH="$HOME/.local/bin:$PATH"
st
```

Supported platforms are Linux x86_64 with glibc 2.35+ (Ubuntu 22.04+) and macOS Apple
Silicon with macOS 15+. The installer needs `curl`, `tar` and a SHA256 checker; macOS
also needs Python 3. Harness CLIs and their accounts are separate. If none is
installed, `st` still opens; install a supported harness and run `st setup` later.
For Claude, first complete its native welcome, account or API-key confirmation and
permission-bypass consent as the same user and profile. Successful auth-status or
print checks alone do not prove that the expert can authenticate.
Neither GitHub nor `gh` is required. Native macOS build and installer tests run in CI;
clean-machine app, launchd and permission setup still need a manual rehearsal.

**[Get started](docs/getting-started.md)** covers first-run questions, the expert,
installation choices and background services. Each person currently runs their own
fleet; start with one machine and add others when you need them.

## Guides

- [Two machines](docs/two-machines.md): connect your machines and check replication.
- [Missions in practice](docs/missions-in-practice.md): revise work, queue runs, and ask for decisions.
- [Talking to agents](docs/talking-to-agents.md): UI, phone, CLI, attachments, and human requests.
- [Build and run the iOS app](docs/ios-app.md): local simulator and iPhone builds, then pairing.
- [Seat lifecycle](docs/seat-lifecycle.md): create, restart, suspend, and import seats.
- [Upgrading st](docs/upgrading-st.md): choose a build and check recovery before changing an existing fleet.
- [GitHub integration](docs/github-integration.md): optional repository intake, review, triage, and landing work.
- [When something is wrong](docs/when-something-is-wrong.md): startup, health, install/upgrade diagnosis, and stop reasons.

## Reference

- [Binary releases](docs/st3/binary-releases.md): archive contents, versions and verification.
- [Nix and Home Manager](docs/getting-started.md#with-nix-and-home-manager)
- [Daemon details](docs/getting-started.md#daemon-details)
- [macOS installation and signing](docs/st3/macos-installation.md)
- [Documentation index](docs/st3/README.md)
- [Runnable examples](examples/st3/README.md)
- [Guided CLI tour](docs/st3/cli-guided-tour.md)
- [KDL lifecycle](docs/st3/kdl-lifecycle.md)

[Contributing](docs/development.md#contributing) covers source builds, checks, and repository layout.
This repository also carries st2, the previous generation; see [its guide](README.st2.md).

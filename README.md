# Small Talk

Small Talk (`st`) runs coding agents as durable seats and hands them work as missions. Its graph
records seats, missions, steps, messages, and decisions, so work survives harness, daemon, and
machine restarts. One daemon runs on each machine; your machines can join a fleet.

Use the terminal UI, CLI, or iOS app to follow work and talk to agents. Small Talk observes each
harness through its driver; missions carry the goals and constraints for the work. Each person
currently runs their own fleet.

**[Get started](docs/getting-started.md)** — install Small Talk and give your first agent a mission.

Platforms/status: Linux x86_64 and macOS Apple Silicon; fresh-Mac setup is not yet verified, and macOS CI is currently disabled.

## Guides

- [Two machines](docs/two-machines.md): connect your machines and check replication.
- [Missions in practice](docs/missions-in-practice.md): revise work, queue runs, and ask for decisions.
- [Talking to agents](docs/talking-to-agents.md): UI, phone, CLI, attachments, and human requests.
- [Build and run the iOS app](docs/ios-app.md): local simulator and iPhone builds, then pairing.
- [Seat lifecycle](docs/seat-lifecycle.md): create, restart, suspend, and import seats.
- [Upgrading st](docs/upgrading-st.md): install the same build everywhere and check recovery.
- [GitHub integration](docs/github-integration.md): repository intake, review, triage, and landing work.
- [When something is wrong](docs/when-something-is-wrong.md): health, work, usage, and stop reasons.

## Reference

- [Documentation index](docs/st3/README.md)
- [Runnable examples](examples/st3/README.md)
- [Guided CLI tour](docs/st3/cli-guided-tour.md)
- [KDL lifecycle](docs/st3/kdl-lifecycle.md)
- [Running st with omp](docs/st3/omp.md)

[Contributing](docs/development.md#contributing) covers source builds, checks, and repository layout.

This repository also carries st2, the previous generation; see [its guide](README.st2.md).

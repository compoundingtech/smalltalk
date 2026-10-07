# Smalltalk

Smalltalk turns your coding agents into a team that doesn't need babysitting. Describe the work once as a mission, with its goals, its constraints and the checks that prove it's done right, not just done. Agents on your own machines carry it through, survive restarts, and bring you only the decisions that are yours to make. You plan and decide; your agents do the rest, all day and overnight.

`st` runs agents as durable seats and records work, messages, and decisions in a graph.
Publish seats, missions, and schedules with `st apply FILE...`; add `--dry-run` to validate
and preview, or `--dry-run --check` to run the exec gates too.
One daemon runs on each machine; your machines can join a fleet. Use the terminal UI, CLI,
or iOS app to follow work and talk to agents. Each person currently runs their own fleet.

**[Get started](docs/getting-started.md)** — install Smalltalk and give your first agent a mission.

Platforms/status: Linux x86_64 and macOS Apple Silicon; fresh-Mac setup is not yet verified, and macOS CI is currently disabled.

## Guides

- [Two machines](docs/two-machines.md): connect your machines and check replication.
- [Missions in practice](docs/missions-in-practice.md): revise work, queue runs, and ask for decisions.
- [Talking to agents](docs/talking-to-agents.md): UI, phone, CLI, attachments, and human requests.
- [Build and run the iOS app](docs/ios-app.md): local simulator and iPhone builds, then pairing.
- [Seat lifecycle](docs/seat-lifecycle.md): create, restart, suspend, and import seats.
- [0.x compatibility](docs/st3/compatibility.md): upgrade baselines, client/harness boundaries and platform proof.
- [Upgrading st](docs/upgrading-st.md): install the same build everywhere and check recovery.
- [GitHub integration](docs/github-integration.md): repository intake, review, triage, and landing work.
- [When something is wrong](docs/when-something-is-wrong.md): startup, install/upgrade diagnosis, sanitized issue reports, health and stop reasons.

## Reference

- [Documentation index](docs/st3/README.md)
- [Runnable examples](examples/st3/README.md)
- [Guided CLI tour](docs/st3/cli-guided-tour.md)
- [KDL lifecycle](docs/st3/kdl-lifecycle.md)
- [Running st with omp](docs/st3/omp.md)

[Contributing](docs/development.md#contributing) covers source builds, checks, and repository layout.

This repository also carries st2, the previous generation; see [its guide](README.st2.md).

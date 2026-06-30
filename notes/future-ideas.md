---
date: 2026-05-12
audience: future-us
purpose: collect "agreed this is interesting, parked for later" ideas so they don't get lost
---

# Future ideas — parked

Things we've discussed but decided not to build yet. Listed so the next round of work has a menu to pull from instead of re-discovering each one.

> **Naming note.** Written when the project was named `coord`. Some
> sections below describe features the `tasks/` and `journal/` folders
> would have grown — both folders were removed in brief-009 items 1+2,
> so those specific ideas are now retired (kept for history).

## Structural role enforcement (manager / worker)

Already documented in `agent-roles.md`. Today we use prompts + memory + `agents.md`. Future options if drift becomes a real problem:

- PreToolUse soft-warning hook when the manager is about to Edit/Write in implementation paths.
- ~~External task tracker (could be coord's `tasks/` folder once that ships — manager polls `coord tasks worker-claude --status doing` instead of `pty peek`)~~. **Retired** — `tasks/` folder removed in brief-009 item 1.
- Per-role env vars surfaced as session tags.

## ~~`coord_task_*` / `coord_journal_*` / `coord_overview` MCP tools~~ (retired)

**Retired in brief-009 items 1+2** (tasks/ and journal/ folders removed). The original idea was to expose those folders' verbs through MCP once they settled; since the folders themselves are gone, this idea is obsolete. The `st_overview` / `coord_overview` MCP tool stayed parked but the underlying `st overview` CLI verb still ships.

The MCP layer today covers the message tools (`st_msg_send` / `coord_msg_send`, `st_msg_ls` / `coord_msg_ls`, `st_msg_read` / `coord_msg_read`, `st_msg_archive` / `coord_msg_archive`, `st_msg_thread` / `coord_msg_thread`, plus `st_msg_reply` / `coord_msg_reply` in channel mode), peer discovery (`st_agents` / `coord_members`), and the resource surface (`st_resource_*` / `coord_resource_*`).

## Pi end-to-end agent-to-agent demo (the 5-agent scene running for real)

The walkthrough's "A bigger demo" section describes a 5-agent setup (manager-claude, worker-claude, pi-agent, codex-agent, myobie) sharing one `$ST_ROOT`. The folder layout exists in `/tmp/smalltalk-demo/`; what's missing is actually configuring each agent's harness and running all five sessions for real. Could be done as a demo-script (`examples/full-demo/setup.sh` + a tmux/pty layout that boots all five). Useful as both onboarding material and a stress test of the integration.

## Trim convergence across machines (the deferred bug)

LAYOUT.md mentions this. `archive trim` is local-only; a peer that hasn't trimmed yet will resurrect trimmed files on the next sync. Workaround: schedule trim on a similar cadence everywhere. Real fix: tombstone protocol or scoped `--delete` for archive. Worth doing if cross-machine deployment actually happens.

## `st peer add` verb

`peers.yaml` is hand-edited today. A CLI verb for adding/removing peers would be nice once peer counts grow. Currently fine for the local-mode and single-machine demos.

## Full leniency for `st message ls <missing-agent>`

Today: `ls dave` where `dave/inbox` exists but is empty returns `# 0 messages in inbox` exit 0, while `ls eve` where no `eve/` folder exists at all errors `agent folder missing for eve` exit 1. Soft asymmetry observed during the brief-017a DX walkthrough.

**Why parked, not fixed:** the error path is the typo-catching signal. `st message ls dvae` (typo) currently errors loudly. Full leniency would silently return "0 messages" and hide the typo. That's the right tradeoff for the cross-agent read surface — we *want* the error when an agent doesn't exist, even though we want the empty-inbox case to succeed quietly.

If it ever becomes a real friction (e.g. agents need to probe-without-erroring whether a peer is hosted), we can revisit. The escape hatch today is `st agents` — that just doesn't list a non-existent agent.

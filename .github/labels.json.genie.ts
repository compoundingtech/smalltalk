import { commonLabels, githubLabels } from '../repos/effect-utils/genie/external.ts'

// Compose only labels already used here; unrelated shared areas and CI grants are not enabled.
const sharedNames: Record<string, true> = {
  'area:ci': true,
  'origin:agent': true,
  'origin:janitor': true,
  'state:blocked': true,
  'state:needs-research': true,
  'state:triage': true,
  'type:agent-tooling': true,
  'type:bug': true,
  'type:chore': true,
  'type:docs': true,
  'type:epic': true,
  'type:feature': true,
  'type:incident': true,
  'type:rca': true,
}

export default githubLabels({
  labels: [
    ...commonLabels.filter(({ name }) => sharedNames[name] === true),
    {
      name: "state:open-design-question",
      color: "6e7781",
      description: "Unresolved design choice requiring explicit alignment before implementation · Set: manual",
    },
    {
      name: "area:ding",
      color: "1d76db",
      description: "DING delivery: inbox notice into a running agent · Set: manual",
    },
    {
      name: "area:driver",
      color: "1d76db",
      description: "Harness drivers: launch, MCP, app-server, native delivery · Set: manual",
    },
    {
      name: "area:agent-spec",
      color: "1d76db",
      description: "Agent Spec format, parsing, and the agent-spec crate · Set: manual",
    },
    {
      name: "area:catalog",
      color: "1d76db",
      description: "Catalog structure, declarations, transactions, and admission · Set: manual",
    },
    {
      name: "area:message",
      color: "1d76db",
      description: "Native message bus, inbox, archive, and receipts · Set: manual",
    },
    {
      name: "area:presence",
      color: "1d76db",
      description: "Presence, status records, and heartbeats · Set: manual",
    },
    {
      name: "area:reconcile",
      color: "1d76db",
      description: "Supervisor run loop, lifecycle, restart, park, and teardown · Set: manual",
    },
    {
      name: "area:exec",
      color: "1d76db",
      description: "Exec backend and process-group management · Set: manual",
    },
    {
      name: "area:pty",
      color: "1d76db",
      description: "PTY sessions and terminal integration · Set: manual",
    },
    {
      name: "area:doctor",
      color: "1d76db",
      description: "doctor, validate, and task inventory diagnostics · Set: manual",
    },
    {
      name: "area:resource",
      color: "1d76db",
      description: "Typed Resource bindings and linked records · Set: manual",
    },
    {
      name: "area:eval",
      color: "1d76db",
      description: "st2 eval harness and fixtures · Set: manual",
    },
    {
      name: "area:vrs",
      color: "1d76db",
      description: "VRS documentation system (vision/requirements/spec) · Set: manual",
    },
    {
      name: "area:identity",
      color: "1d76db",
      description: "Agent, session, run, and launch-generation identity · Set: manual",
    },
    {
      name: "area:telemetry",
      color: "1d76db",
      description: "OpenTelemetry and observability instrumentation · Set: manual",
    },
    {
      name: "harness:claude",
      color: "a371f7",
      description: "Claude Code-specific behavior · Set: manual",
    },
    {
      name: "harness:codex",
      color: "a371f7",
      description: "Codex-specific behavior · Set: manual",
    },
    {
      name: "harness:neutral",
      color: "a371f7",
      description: "Harness-neutral core that must not encode provider specifics · Set: manual",
    },
    {
      name: "macos-ci",
      color: "5319e7",
      description: "Run the optional macOS CI workflow for this pull request · Set: manual",
    },
    {
      name: "ci-priority",
      color: "D73A4A",
      description: "Urgent trusted PR: reserve CI capacity ahead of ordinary PRs · Set: manual",
    },
    // This narrow draft exception is distinct from a general open design question.
    {
      name: 'needs-design-input',
      color: '6e7781',
      description: 'Draft needs Nathan’s design feedback only; never authorizes merge · Set: author or maintainer',
    },
    // Non-draft is the ready-set signal. Only the highest priority needs an extra label.
    {
      name: 'priority:p0',
      color: 'b60205',
      description: 'First in the capped ready set: instant web app or memory/OOM fix · Set: author or maintainer',
    },
  ],
  // Keep every live label, including historical-only labels and both CI selectors.
  deprecated: [],
  legacyMigrations: [],
})

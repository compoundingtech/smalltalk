/** Story fixtures in the shape st serves: the agent text matches `render_agent_desired_kdl` output with env values redacted. */
export const agentDeclarationKdl = `version 2
agent "example/ui-design" {
    accountable "person/avery"
    description "Design seat for the Fractal web client. Works only inside its seat folder."
    workspace "/srv/work/example/ui-design"
    harness "omp" {
        model "anthropic/claude-opus-5-5"
        effort "medium"
    }
    env {
        GITHUB_TOKEN "<redacted>"
        ST_PERMISSIONS "<redacted>"
        ST_SEAT_PRIORITY "<redacted>"
    }
}
`

/** A mission declaration as authored; st serves no KDL for missions today, so this is a kit-only fixture. */
export const missionDeclarationKdl = `version 2
mission "example/ui-design/kdl-view" state="ready" timeout="3h" report-to="agent/example/ui-design" {
  goal "Canonical KDL viewer modal for agents and missions"
  constraint "Builder follows build-brief.md and gate follows gate-brief.md. Never push."
  step "start-builder" {
    agentless
    agent "builder" {
      workspace "/srv/work/example/kdl-view"
      harness "omp" {
        model "openai-codex/gpt-6.1-sol"
        effort "medium"
      }
    }
  }
  loop "refine" timeout="3h" {
    depends-on { step "start-builder" completed }
    max-rounds 3
    until {
      gate "the latest gate verdict is GO" {
        exec "[ -f gate/report-r\${loop.round}.json ] || exit 1"
        host "local"
        time-limit "1m"
      }
    }
    round {
      step "build" timeout="4h" {
        assigned-to "agent/\${ST_ROOT_MISSION_RUN_ID}/builder"
        goal "Round \${loop.round}: build or fix per build-brief.md, then write build-r\${loop.round}.md."
      }
      step "gate" timeout="5h" {
        assigned-to "agent/\${ST_MISSION_RUN}/gate"
        depends-on { step "build" completed }
        goal "Round \${loop.round}: gate per gate-brief.md."
      }
    }
    on-exhausted { fail; }
  }
}
`

/** The structured fallback the web client shows for a mission: the published definition as JSON. */
export const missionDeclarationJson = `${JSON.stringify({
  id: 'example/ui-design/kdl-view',
  subject: 'mission/example/ui-design/kdl-view',
  state: 'ready',
  revision: 'rev-7f3c9e1a',
  max_active_runs: 1,
  timeout_ms: 10800000,
  report_to: 'agent/example/ui-design',
  goals: ['Canonical KDL viewer modal for agents and missions'],
  constraints: ['Builder follows build-brief.md and gate follows gate-brief.md. Never push.'],
  gates: [{ predicate: 'mechanical', name: 'the latest gate verdict is GO', command: '[ -f gate/report-r1.json ] || exit 1', host: 'local', workspace: '/srv/work/example/kdl-view', environment: { GH_TOKEN: '<redacted>' }, time_limit_ms: 60000 }],
  declarations_kdl: '<withheld: embedded declarations can carry environment values>',
  display_order: ['start-builder', 'build', 'gate'],
}, null, 2)}\n`

/** A mission long enough to scroll in both directions, with a four-digit gutter. */
export const longDeclarationKdl = `version 2
mission "example/fleet-sweep" state="ready" timeout="12h" report-to="agent/example/fleet-steward" {
  goal "Sweep every repository in the fleet and file one finding per repository; this goal line is deliberately long so it overflows the panel width at narrow viewports."
${Array.from({ length: 170 }, (_, index) => {
  const name = `sweep-${String(index).padStart(3, '0')}`
  return `  step "${name}" timeout="20m" {
    assigned-to "agent/\${ST_MISSION_RUN}/sweeper"
    goal "Sweep repository ${index + 1} and file one finding."
${index === 0 ? '' : `    depends-on { step "sweep-${String(index - 1).padStart(3, '0')}" completed }\n`}  }`
}).join('\n')}
}
`

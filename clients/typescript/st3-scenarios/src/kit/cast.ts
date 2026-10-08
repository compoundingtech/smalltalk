import { fork, int, pick, sample, type Rng } from './rng.ts'
import * as vocabulary from './vocabulary/index.ts'

export interface CastAgent {
  readonly id: string
  /** Display name, distinct from the identity. */
  readonly name: string
  readonly project: string
  readonly role: vocabulary.Role
  readonly driver: string
  readonly model: string
  readonly host: CastHost
  readonly workspace: string
  readonly branch: string
  readonly runtime: string
  readonly runtimeId: string
  readonly incarnation: string
  readonly session: string
  readonly terminal: string
  readonly terminalRuntime: string
  readonly terminalIncarnation: string
}

export interface CastHost {
  readonly id: string
  readonly name: string
  readonly machine: string
}

export interface CastPerson {
  readonly id: string
  readonly name: string
}

export interface CastStep {
  readonly path: string
  readonly title: string
  readonly stepRun: string
  readonly work: string
}

export interface CastMission {
  readonly id: string
  readonly title: string
  readonly run: string
  readonly generation: string
  readonly steps: CastStep[]
}

export interface Cast {
  readonly project: string
  readonly repository: string
  readonly agents: CastAgent[]
  readonly people: CastPerson[]
  readonly hosts: CastHost[]
  readonly missions: CastMission[]
}

export interface CastSpec {
  readonly project: string
  readonly roles: readonly vocabulary.Role[]
  readonly hosts: number
  readonly people: number
  readonly missions: readonly { readonly slug: string; readonly title: string; readonly steps: readonly string[] }[]
}

const titleCase = (text: string) => text.charAt(0).toUpperCase() + text.slice(1)

/** Draws a world's cast; every slice of the world references only these members. */
export const drawCast = (rng: Rng, spec: CastSpec): Cast => {
  const hostWords = sample(fork(rng, 'hosts'), vocabulary.hostWords, spec.hosts)
  const hosts = hostWords.map((word) => ({ id: `host/${word}`, name: word, machine: `machine/${word}` }))
  const agents = spec.roles.map((role, index): CastAgent => {
    const r = fork(rng, `agent/${role}`)
    const harness = pick(r, vocabulary.harnesses)
    const id = `agent/example/${spec.project}/${role}`
    const runtimeId = `rt-${spec.project}-${role}-${int(r, 100, 999)}`
    return {
      id,
      name: `${titleCase(spec.project)} ${vocabulary.roles[role]}`,
      project: spec.project,
      role,
      driver: harness.driver,
      model: harness.model,
      host: hosts[index % hosts.length]!,
      workspace: `~/src/${spec.project}/.worktrees/${role}`,
      branch: `${role}/${spec.missions[0]?.slug ?? 'main'}`,
      runtime: `runtime/example-${spec.project}-${role}`,
      runtimeId,
      incarnation: `${runtimeId}:${int(r, 1, 9)}`,
      session: `session/example-${spec.project}-${role}`,
      terminal: `terminal/example-${spec.project}-${role}`,
      terminalRuntime: `runtime/example-${spec.project}-${role}-shell`,
      terminalIncarnation: `pty-${int(r, 10, 99)}:1`,
    }
  })
  const missions = spec.missions.map((mission): CastMission => {
    const run = `mission-run/example/${spec.project}/${mission.slug}/1`
    return {
      id: `mission/example/${spec.project}/${mission.slug}`,
      title: mission.title,
      run,
      generation: `run-generation/example/${spec.project}/${mission.slug}/1/g1`,
      steps: mission.steps.map((title, index) => {
        const path = `step-${index + 1}`
        return {
          path,
          title,
          stepRun: `step-run/example/${spec.project}/${mission.slug}/1/${path}`,
          work: `work/example/${spec.project}/${mission.slug}/1/${path}`,
        }
      }),
    }
  })
  return {
    project: spec.project,
    repository: `~/src/${spec.project}`,
    agents,
    people: vocabulary.people.slice(0, spec.people).map((person) => ({ ...person })),
    hosts,
    missions,
  }
}

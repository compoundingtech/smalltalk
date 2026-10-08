import { fork, int, pick, sample, type Rng } from './rng.ts'
import * as vocabulary from './vocabulary/index.ts'

export interface CastAgent {
  /** Stable identity suffix; independent of the semantic role. */
  readonly key: string
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

export interface CastAgentSpec {
  readonly key: string
  readonly role: vocabulary.Role
  readonly name?: string
  readonly workspace?: string
  readonly branch?: string
}

export interface CastSpec {
  readonly project: string
  readonly roles?: readonly vocabulary.Role[]
  readonly agents?: readonly CastAgentSpec[]
  readonly hosts: number
  readonly people: number
  readonly missions: readonly { readonly slug: string; readonly title: string; readonly steps: readonly string[] }[]
}

const titleCase = (text: string) => text.charAt(0).toUpperCase() + text.slice(1)

/** Draws a world's cast; every slice of the world references only these members. */
export const drawCast = (rng: Rng, spec: CastSpec): Cast => {
  if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(spec.project)) throw new Error('cast project must be a public slug')
  if ((spec.roles === undefined) === (spec.agents === undefined)) throw new Error('cast needs exactly one of roles or agents')
  const members: readonly CastAgentSpec[] = spec.agents ?? (spec.roles ?? []).map((role) => ({ key: role, role }))
  if (new Set(members.map((member) => member.key)).size !== members.length) throw new Error('cast agent keys must be unique')
  for (const member of members) {
    if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(member.key)) throw new Error('cast agent key must be a public slug')
    if (member.workspace !== undefined && ((member.workspace !== `~/src/${spec.project}` && !member.workspace.startsWith(`~/src/${spec.project}/`)) || member.workspace.split('/').includes('..'))) {
      throw new Error('cast workspace must stay under its public project')
    }
  }
  const hostWords = sample(fork(rng, 'hosts'), vocabulary.hostWords, spec.hosts)
  const hosts = hostWords.map((word) => ({ id: `host/${word}`, name: word, machine: `machine/${word}` }))
  if (hosts.length === 0) throw new Error('cast needs a host')
  const agents = members.map(({ key, role, name, workspace, branch }, index): CastAgent => {
    const r = fork(rng, `agent/${key}`)
    const harness = pick(r, vocabulary.harnesses)
    const id = `agent/example/${spec.project}/${key}`
    const runtimeId = `rt-${spec.project}-${key}-${int(r, 100, 999)}`
    return {
      key,
      id,
      name: name ?? `${titleCase(spec.project)} ${vocabulary.roles[role]}`,
      project: spec.project,
      role,
      driver: harness.driver,
      model: harness.model,
      host: hosts[index % hosts.length]!,
      workspace: workspace ?? `~/src/${spec.project}/.worktrees/${key}`,
      branch: branch ?? `${key}/${spec.missions[0]?.slug ?? 'main'}`,
      runtime: `runtime/example-${spec.project}-${key}`,
      runtimeId,
      incarnation: `${runtimeId}:${int(r, 1, 9)}`,
      session: `session/example-${spec.project}-${key}`,
      terminal: `terminal/example-${spec.project}-${key}`,
      terminalRuntime: `runtime/example-${spec.project}-${key}-shell`,
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

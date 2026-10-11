import type { Agent, AgentTodo, HarnessPhase } from '@smalltalk/st3-client/schema'
import { Option } from 'effect'
import * as Atom from 'effect/reactivity/Atom'

import type { DataSource, Feed } from '../../data/source.ts'

/** Todo observation availability or accepted progress with phase, source totals and freshness. */
export type TodoState =
  | { readonly _tag: 'Waiting' }
  | { readonly _tag: 'Unavailable'; readonly detail: string }
  | { readonly _tag: 'Missing' }
  | {
      readonly _tag: 'Observed'
      readonly todo: AgentTodo
      readonly stale: boolean
      readonly phase: HarnessPhase | undefined
      readonly completed: number
      readonly total: number
    }

/** Full-source totals remain authoritative when the displayed task list is truncated. */
export const projectTodos = ({
  feed,
  agentRef,
}: {
  feed: Feed<readonly Agent[]>
  agentRef: string
}): TodoState => {
  if (feed._tag !== 'Observed') return feed
  const agent = feed.value.find((row) => row.id === agentRef)
  const todo = agent === undefined ? undefined : Option.getOrUndefined(agent.todo)
  if (todo === undefined) return { _tag: 'Missing' }
  const { snapshot } = todo
  const incarnation = agent === undefined ? undefined : Option.getOrUndefined(agent.incarnation_id)
  const phase =
    snapshot.phases.find((candidate) =>
      candidate.tasks.some((task) => task.status === 'in_progress'),
    ) ??
    snapshot.phases.find((candidate) =>
      candidate.tasks.some((task) => task.status === 'blocked'),
    ) ??
    snapshot.phases.find((candidate) => candidate.tasks.some((task) => task.status === 'pending'))
  const { completed, pending, in_progress, blocked, abandoned = 0 } = snapshot.totals
  return {
    _tag: 'Observed',
    todo,
    phase,
    completed,
    total: completed + pending + in_progress + blocked + abandoned,
    stale:
      todo.stale ||
      feed.freshness === 'stale' ||
      (incarnation !== undefined && incarnation !== snapshot.incarnation_id),
  }
}

type TodoFamily = (agentRef: string) => Atom.Atom<TodoState>
const sources = new WeakMap<DataSource, TodoFamily>()

/** Both surfaces share one derivation over the already-followed generated agent rows. */
export const agentTodosAtom = ({
  source,
  agentRef,
}: {
  source: DataSource
  agentRef: string
}): Atom.Atom<TodoState> => {
  let family = sources.get(source)
  if (family === undefined) {
    family = Atom.family((ref: string) =>
      Atom.make((get) => projectTodos({ feed: get(source.agents), agentRef: ref })),
    )
    sources.set(source, family)
  }
  return family(agentRef)
}

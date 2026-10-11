import { Schema } from 'effect'

import { SubjectAddress, type PresentationId } from '../resources/contract.ts'
import { LayoutJson, subjectKey, type SplitNode } from '../shell/state.ts'

// One-shot reader arm for authoritative local working context. Remove when every persisted
// :window, :window:<id> and :layout:last-used entry in wf.ui@1 reports entry version >= 2.
const LegacyInput = Schema.Struct({ ref: Schema.String, editor: Schema.String })
const StoredInput = Schema.Union([SubjectAddress, LegacyInput])
const { docks: _docks, ...layoutFields } = LayoutJson.fields
const StoredLayout = Schema.Struct({
  ...layoutFields,
  groups: Schema.Record(
    Schema.String,
    Schema.Struct({
      editors: Schema.Array(Schema.Struct({ input: StoredInput })),
      active: Schema.NullOr(Schema.String),
    }),
  ),
})
const presentations: Readonly<Record<string, PresentationId>> = {
  'wf.agentSession': 'detail',
  'wf.terminal': 'detail',
  'wf.mission': 'detail',
  'wf.subject': 'detail',
  'wf.monitor': 'detail',
  'wf.agentUsage': 'overview',
  'wf.agentResources': 'resources',
}

const convertInput = (input: typeof StoredInput.Type): SubjectAddress => {
  if ('presentation' in input) return input
  const presentation = Object.hasOwn(presentations, input.editor)
    ? presentations[input.editor]
    : undefined
  // Unknown old contributions remain an explicit storage error, never an invented detail route.
  return Schema.decodeUnknownSync(SubjectAddress)({ ref: input.ref, presentation })
}

const repairSplits = (node: SplitNode): SplitNode => {
  if (node._tag === 'group') return node
  const valid = node.sizes.length === node.children.length && node.sizes.every((size) => size > 0)
  const total = node.sizes.reduce((sum, size) => sum + size, 0)
  const sizes = valid
    ? Math.abs(total - 1) < Number.EPSILON * node.sizes.length
      ? node.sizes
      : node.sizes.map((size) => size / total)
    : node.children.map(() => 1 / node.children.length)
  return { ...node, sizes, children: node.children.map(repairSplits) }
}

/** Pure conversion; the canonical persistence opener rewrites the versioned entry once. */
export const migrateWindowState = (value: unknown): unknown => {
  if (value === null) return null
  const bytes = Schema.decodeUnknownSync(Schema.Record(Schema.String, Schema.Unknown))(value)
  const stored = Schema.decodeUnknownSync(
    Schema.Struct({ layouts: Schema.Record(Schema.String, StoredLayout) }),
  )(value)
  return {
    ...bytes,
    layouts: Object.fromEntries(
      Object.entries(stored.layouts).map(([id, layout]) => [
        id,
        {
          ...layout,
          editorArea: repairSplits(layout.editorArea),
          groups: Object.fromEntries(
            Object.entries(layout.groups).map(([groupId, group]) => {
              const entries = group.editors.map((entry) => ({
                old: entry.input,
                input: convertInput(entry.input),
              }))
              const active = entries.find(
                ({ old, input }) =>
                  ('editor' in old ? `${old.editor}|${old.ref}` : subjectKey(input)) ===
                  group.active,
              )
              return [
                groupId,
                {
                  editors: entries.map(({ input }) => ({ input })),
                  active: active === undefined ? group.active : subjectKey(active.input),
                },
              ]
            }),
          ),
        },
      ]),
    ),
  }
}

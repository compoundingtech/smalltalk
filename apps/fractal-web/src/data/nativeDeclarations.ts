import { Schema } from 'effect'
import type { AgentDeclaration } from '@smalltalk/st3-client/schema'

/** Public-safe declared intent: never retain raw paths, argv, env values or reference targets. */
export interface NativeDeclarations {
  readonly agentRef: string
  readonly revision: string
  readonly bindings: readonly { readonly kind: string; readonly count: number }[]
  readonly declaredModel?: string
}
const scalar = Schema.Union([Schema.String, Schema.Number, Schema.Boolean, Schema.Null])
const nodeSchema = Schema.Struct({
  name: Schema.String,
  arguments: Schema.optionalKey(Schema.Array(scalar)),
  properties: Schema.optionalKey(Schema.Record(Schema.String, scalar)),
  children: Schema.optionalKey(Schema.Array(Schema.Unknown)),
})
const references: Readonly<Record<string, true>> = { workspace: true, checkout: true, under: true, 'bind-terminal': true, 'mission-authority': true, 'queue-authority': true, 'seat-authority': true, 'agent-authority': true }

/** Validate every node, even excluded ones; only allowlisted reference categories escape. */
export const projectNativeDeclarations = ({ declaration }: { readonly declaration: AgentDeclaration }): NativeDeclarations | undefined => {
  const counts = new Map<string, number>()
  let declaredModel: string | undefined
  let remaining = 2048
  const visit = (value: unknown, depth: number, parent?: string): boolean => {
    if (--remaining < 0 || depth > 32) return false
    const decoded = Schema.decodeUnknownOption(nodeSchema)(value)
    if (decoded._tag === 'None') return false
    const node = decoded.value
    if (depth === 0 && node.name !== 'agent') return false
    if (depth === 1 && references[node.name] === true) counts.set(node.name, (counts.get(node.name) ?? 0) + 1)
    if (depth === 2 && parent === 'harness' && (node.name === 'account' || node.name === 'account-pool')) {
      const kind = `harness ${node.name}`
      counts.set(kind, (counts.get(kind) ?? 0) + 1)
    }
    if (depth === 2 && parent === 'harness' && node.name === 'model') {
      const model = node.arguments?.[0]
      if (typeof model === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._:/-]{0,127}$/.test(model) && model !== '.unknown') declaredModel = model
    }
    return (node.children ?? []).every(child => visit(child, depth + 1, node.name))
  }
  if (!visit(declaration.tree, 0)) return undefined
  return { agentRef: declaration.id, revision: declaration.revision,
    bindings: [...counts].map(([kind, count]) => ({ kind, count })),
    ...(declaredModel === undefined ? {} : { declaredModel }),
  }
}

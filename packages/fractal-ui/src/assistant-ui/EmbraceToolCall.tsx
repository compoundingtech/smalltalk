import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { makeAssistantToolUI, useAuiState } from '@assistant-ui/react'
import type { ToolCallMessagePartProps } from '@assistant-ui/react'
import { Button, Disclosure, DisclosurePanel } from 'react-aria-components'
import type { ConversationItem } from './embrace-converter'
import { accentTokens, tokens, geometry, density } from './embrace-tokens.stylex'
import { EmbraceDiffPreview, EmbraceMarkdownPreview } from './EmbraceToolPreview'
import { embraceToolKind as toolKind, embraceToolNeedsAttention, type EmbraceToolKind as ToolKind } from './embrace-tool-attention'
export { embraceToolNeedsAttention } from './embrace-tool-attention'
import { toolDiffs, toolFields, toolHumanSummary, toolOutput, toolString } from './embrace-tool-preview'

export type ToolVariant = 'rows' | 'grouped' | 'cards'
export type EmbraceToolCallItem = Extract<ConversationItem, { _tag: 'ToolCall' }>
export type EmbraceToolStatus = EmbraceToolCallItem['status']
export const EmbraceToolVariantContext = React.createContext<ToolVariant>('rows')

/** Source status wins: archived interrupted calls must not become permanent running indicators. */
export const embraceToolStatus = (part: ToolCallMessagePartProps, item?: EmbraceToolCallItem): EmbraceToolStatus => {
  if (item !== undefined) return item.status
  if (part.isError) return 'error'
  if (part.status.type === 'incomplete') return part.status.reason === 'error' ? 'error' : 'interrupted'
  if (part.status.type === 'running' || part.status.type === 'requires-action' || part.isPreliminary) return 'running'
  return 'success'
}

const verbs: Record<ToolKind, readonly [string, string]> = {
  read: ['Read', 'Reading'], edit: ['Edited', 'Editing'], write: ['Wrote', 'Writing'],
  search: ['Searched', 'Searching'], run: ['Ran', 'Running'], web: ['Fetched', 'Fetching'],
  question: ['Asked', 'Awaiting answer'], task: ['Delegated', 'Delegating'], other: ['Called', 'Calling'],
}

/** assistant-ui Parts tools.Fallback compatible renderer; registrations share this implementation. */
export const EmbraceToolCall = (props: ToolCallMessagePartProps & { readonly variant?: ToolVariant }) => {
  const item = useAuiState((state) => {
    const candidate = state.message.metadata.custom.item as ConversationItem | undefined
    return candidate?._tag === 'ToolCall' && candidate.callId === props.toolCallId ? candidate : undefined
  })
  const inheritedVariant = React.useContext(EmbraceToolVariantContext)
  return <ToolPresentation
    name={item?.name ?? props.toolName}
    input={item?.input ?? props.args}
    output={item !== undefined ? item.result?.content : props.result}
    mediaType={item?.result?.mediaType}
    status={embraceToolStatus(props, item)}
    variant={props.variant ?? inheritedVariant}
    callSeen={item?.callSeen ?? true}
  />
}

/** Also renders a source item outside a message scope, without manufacturing runtime callbacks. */
export const EmbraceToolItem = ({ item, variant }: { readonly item: EmbraceToolCallItem; readonly variant?: ToolVariant }) => {
  const inheritedVariant = React.useContext(EmbraceToolVariantContext)
  return <ToolPresentation name={item.name} input={item.input} output={item.result?.content}
    mediaType={item.result?.mediaType} status={item.status} variant={variant ?? inheritedVariant} callSeen={item.callSeen} />
}

export type EmbraceToolRunProps = {
  /** A single contiguous run supplied by the transcript owner, not one group per tool part. */
  readonly items: readonly EmbraceToolCallItem[]
  readonly renderItem?: (item: EmbraceToolCallItem) => React.ReactNode
  readonly variant?: ToolVariant
  /** Group chronological tool chrome without hiding any call; the turn owner controls settlement. */
  readonly forceExpanded?: boolean
}

/** One disclosure controls all routine calls; promoted calls never leave chronological order. */
export const EmbraceToolRun = ({ items, renderItem, variant, forceExpanded = false }: EmbraceToolRunProps) => {
  const inheritedVariant = React.useContext(EmbraceToolVariantContext)
  const resolvedVariant = variant ?? inheritedVariant
  const [expanded, setExpanded] = React.useState(false)
  const listId = React.useId()
  if (items.length === 0) return null
  const attention = items.map(embraceToolNeedsAttention)
  const routineCount = attention.reduce((count, promoted) => count + (promoted ? 0 : 1), 0)
  const runningCount = items.reduce((count, item) => count + (item.status === 'running' ? 1 : 0), 0)
  const errorCount = items.reduce((count, item) => count + (item.status === 'error' ? 1 : 0), 0)
  const interruptedCount = items.reduce((count, item) => count + (item.status === 'interrupted' ? 1 : 0), 0)
  const grouped = !forceExpanded && resolvedVariant === 'grouped' && routineCount > 0 && runningCount === 0 && errorCount === 0 && interruptedCount === 0
  return (
    <EmbraceToolVariantContext.Provider value={resolvedVariant}>
      <section aria-label="Tool run" {...stylex.props(styles.run)} data-tool-run-size={items.length}>
        {grouped ? (
          <Button aria-expanded={expanded} aria-controls={listId} onPress={() => setExpanded((value) => !value)} {...stylex.props(styles.groupTrigger)}>
            <span aria-hidden="true">{expanded ? '▾' : '▸'}</span>
            <span>{runningCount > 0 ? 'Working' : 'Tool run'} · {items.length} calls · {routineCount} routine</span>
            <span {...stylex.props(styles.groupStatus)}>
              {`${items.length - runningCount - errorCount - interruptedCount} success`}
              {runningCount > 0 ? ` · ${runningCount} running` : ''}
              {errorCount > 0 ? ` · ${errorCount} error` : ''}
              {interruptedCount > 0 ? ` · ${interruptedCount} interrupted` : ''}
              {expanded ? ' · routine work expanded' : ' · routine work collapsed'}
            </span>
          </Button>
        ) : null}
        <div id={listId} {...stylex.props(styles.run)}>
          {items.map((item, index) => (!grouped || expanded || attention[index]) ? (
            <div key={item.id} data-conversation-entry-id={item.id} data-tool-promoted={attention[index] || undefined}>
              {renderItem !== undefined ? renderItem(item) : <EmbraceToolItem item={item} />}
            </div>
          ) : null)}
        </div>
      </section>
    </EmbraceToolVariantContext.Provider>
  )
}

const ToolPresentation = ({ name, input, output, mediaType, status, variant, callSeen }: {
  readonly name: string
  readonly input: unknown
  readonly output: unknown
  readonly mediaType?: string | undefined
  readonly status: EmbraceToolStatus
  readonly variant: ToolVariant
  readonly callSeen: boolean
}) => {
  const kind = toolKind(name)
  const target = toolString(input, ['path', 'file_path', 'pattern', 'query', 'question', 'url', 'description'])
    || (kind === 'run' ? toolString(input, ['command', 'cmd']).trim().split(/\s+/).slice(0, 2).join(' ') : '')
  const label = `${verbs[kind][status === 'running' ? 1 : 0]} ${target || name}`
  const promoted = status === 'error' || status === 'interrupted' || kind === 'edit' || kind === 'write' || kind === 'question'
    || /^(?:diff --git |--- [^\n]+\n\+\+\+ )/m.test(toolOutput(output))
  const header = <>
    <span aria-hidden="true" {...stylex.props(styles.indicator, status === 'error' && styles.error, status === 'interrupted' && styles.warning)}>
      {status === 'running' ? '◌' : status === 'success' ? '✓' : status === 'error' ? '!' : '■'}
    </span>
    <span {...stylex.props(styles.label)} title={label}>{label}</span>
    {!callSeen ? <span {...stylex.props(styles.caption)}>Start not loaded</span> : null}
    <span {...stylex.props(styles.status, status === 'error' && styles.error, status === 'interrupted' && styles.warning)}>{status}</span>
  </>
  if (variant === 'rows') return <div {...stylex.props(styles.row)} data-tool-status={status}>{header}</div>
  return (
    <article {...stylex.props(styles.call, variant === 'cards' && styles.card, promoted && styles.promoted)} data-tool-status={status}>
      <Disclosure key={`${variant}:${promoted ? 'attention' : 'routine'}`} defaultExpanded={variant === 'cards' || promoted}>
        {({ isExpanded }) => <>
          <Button slot="trigger" {...stylex.props(styles.row, styles.trigger)}>
            {header}<span aria-hidden="true">{isExpanded ? '▾' : '▸'}</span>
          </Button>
          <DisclosurePanel>
            {isExpanded ? <ToolDetails name={name} input={input} output={output} mediaType={mediaType} status={status} variant={variant} /> : null}
          </DisclosurePanel>
        </>}
      </Disclosure>
    </article>
  )
}

const ToolDetails = ({ name, input, output, mediaType, status, variant }: {
  readonly name: string; readonly input: unknown; readonly output: unknown
  readonly mediaType?: string | undefined; readonly status: EmbraceToolStatus; readonly variant: ToolVariant
}) => {
  const kind = toolKind(name)
  const outputText = toolOutput(output)
  const diffs = toolDiffs(input, output)
  const path = toolString(input, ['path', 'file_path'])
  const markdown = mediaType === 'text/markdown' || /\.md(?:own)?$/i.test(path)
    ? (status === 'error' ? toolString(input, ['content', 'markdown']) : outputText || toolString(input, ['content', 'markdown']))
    : toolString(output, ['markdown'])
  const question = toolString(input, ['question', 'prompt', 'message'])
  const options = toolFields(input).options
  return <div {...stylex.props(styles.details)}>
    <p {...stylex.props(styles.caption)}>{toolHumanSummary(input, name)} · {status}</p>
    {kind === 'question' ? <section aria-label="Agent question">
      <p {...stylex.props(styles.question)}>{question || 'Agent needs your answer'}</p>
      {Array.isArray(options) ? <ul {...stylex.props(styles.options)}>{options.map((option, index) => <li key={index}>
        {typeof option === 'string' ? option : toolString(option, ['label', 'title', 'value'])}
      </li>)}</ul> : null}
      {status === 'running' && output === undefined ? <p {...stylex.props(styles.caption)}>Awaiting answer</p> : null}
    </section> : null}
    {diffs.length > 0 ? <>
      <p {...stylex.props(styles.caption)}>{status === 'success' ? 'Recorded changes' : status === 'running' ? 'Proposed changes · tool running' : 'Proposed changes · not confirmed applied'}</p>
      <EmbraceDiffPreview diffs={diffs} />
    </> : markdown !== '' ? <EmbraceMarkdownPreview markdown={markdown} /> : null}
    {status === 'error' && outputText !== '' ? <pre {...stylex.props(styles.output, styles.error)}>{outputText}</pre> : null}
    {output === undefined && kind !== 'question' ? <p {...stylex.props(styles.caption)}>
      {status === 'running' ? 'Tool is running; no result yet.' : status === 'interrupted' ? 'Interrupted before a result was recorded.' : 'No result was recorded.'}
    </p> : null}
    <details {...stylex.props(styles.rawDetails)}>
      <summary {...stylex.props(styles.rawSummary)}>Show raw input/output</summary>
      <pre {...stylex.props(styles.output)}>{JSON.stringify({ input, result: output }, null, 2)}</pre>
    </details>
  </div>
}

// These names match the converted source fixture tools exactly. Mount once inside the runtime.
export const EmbraceEditToolUI = makeAssistantToolUI({ toolName: 'edit', render: EmbraceToolCall })
export const EmbraceMarkdownToolUI = makeAssistantToolUI({ toolName: 'write', render: EmbraceToolCall })
export const EmbraceToolRegistrations = () => <><EmbraceEditToolUI /><EmbraceMarkdownToolUI /></>

const styles = stylex.create({
  run: { display: 'flex', flexDirection: 'column', gap: 3, minWidth: 0 },
  groupTrigger: { display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: density.gap, minHeight: `calc(${density.toolHeight} + ${geometry.toolExtraHeight})`, paddingBlock: density.toolY, paddingInline: density.toolX, borderWidth: 1, borderStyle: 'solid', borderColor: tokens.line, borderRadius: geometry.toolRadius, backgroundColor: tokens.recess, color: tokens.ink, fontFamily: geometry.toolFont, fontSize: density.senderSize, textAlign: 'left', cursor: 'pointer', outlineOffset: 3, outlineColor: accentTokens.accent },
  groupStatus: { color: tokens.muted, fontSize: 11 },
  call: { minWidth: 0, color: tokens.ink },
  card: { borderWidth: 1, borderStyle: 'solid', borderColor: tokens.line, borderRadius: geometry.toolRadius, backgroundColor: tokens.panel, overflow: 'hidden' },
  promoted: { borderInlineStartWidth: 2, borderInlineStartStyle: 'solid', borderInlineStartColor: tokens.line },
  row: { display: 'flex', alignItems: 'center', gap: density.gap, minWidth: 0, minHeight: `calc(${density.toolHeight} + ${geometry.toolExtraHeight})`, boxSizing: 'border-box', paddingBlock: density.toolY, paddingInline: density.toolX, color: tokens.ink, fontFamily: geometry.toolFont, fontSize: density.senderSize, lineHeight: 1.5 },
  trigger: { width: '100%', borderWidth: geometry.toolBorder, borderStyle: 'solid', borderColor: tokens.line, borderRadius: geometry.toolRadius, backgroundColor: { default: geometry.toolBackground, ':hover': tokens.recess }, cursor: 'pointer', textAlign: 'left', outlineColor: accentTokens.accent, outlineOffset: -2 },
  indicator: { width: 16, flexShrink: 0, textAlign: 'center', color: tokens.muted },
  label: { flexGrow: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  status: { flexShrink: 0, color: tokens.muted, fontSize: 10, textTransform: 'capitalize' },
  caption: { color: tokens.muted, fontSize: 11, marginBlock: 4 },
  error: { color: tokens.danger },
  warning: { color: tokens.warning },
  details: { display: 'flex', flexDirection: 'column', gap: density.gap, padding: density.toolDetails, minWidth: 0 },
  output: { margin: 0, padding: 8, borderRadius: 4, backgroundColor: tokens.recess, color: tokens.ink, fontSize: 11, lineHeight: 1.55, fontFamily: 'monospace', whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', maxHeight: 280, overflowY: 'auto' },
  question: { margin: 0, fontSize: 13, fontWeight: 600 },
  options: { marginBlock: 5, paddingInlineStart: 20, fontSize: 12 },
  rawDetails: { color: tokens.muted, fontSize: 11 },
  rawSummary: { cursor: 'pointer', outlineColor: accentTokens.accent, outlineOffset: 3 },
})

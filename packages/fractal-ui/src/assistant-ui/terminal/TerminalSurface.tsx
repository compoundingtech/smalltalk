import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { useFocusRing } from 'react-aria'
import { Button } from 'react-aria-components'
import { colorVars as c, typeVars as t, surfaceVars as surface } from '../composition-tokens.stylex'
import { appendLocalHistory, encodeTerminalKey, encodeTerminalPaste, type LocalHistory } from './terminal-behavior'
import type { TerminalSurfaceHandle, TerminalSurfaceProps, TerminalScreen } from './terminal-types'

const blink = stylex.keyframes({ '0%': { opacity: 1 }, '50%': { opacity: 0 }, '100%': { opacity: 1 } })
const defaultFont = { family: "ui-monospace, 'SF Mono', Menlo, Consolas, monospace", sizePx: 13, lineHeightPx: 20, advanceEm: 0.6 }
export function terminalConnectionReason(connection: TerminalSurfaceProps['connection']): string {
  switch (connection.state) {
    case 'live': return 'Waiting for terminal output.'
    case 'connecting': return 'Connecting to the terminal.'
    case 'reconnecting': return connection.detail || 'The connection was interrupted. Reconnecting to the terminal.'
    case 'ended': case 'unavailable': return connection.reason
  }
}
export function TerminalSurface(props: TerminalSurfaceProps) {
  const { focusProps, isFocusVisible } = useFocusRing({ within: true })
  return <Surface {...props} focusProps={focusProps} visibleFocus={props.focusRing !== false && isFocusVisible} />
}
type Props = TerminalSurfaceProps & { focusProps: React.HTMLAttributes<HTMLElement>; visibleFocus: boolean }
type State = { history: LocalHistory; previous: TerminalScreen | null; limit: number; focused: boolean }
class Surface extends React.Component<Props, State> {
  state: State = { history: { lines: [], truncated: false }, previous: this.props.screen, limit: this.props.scrollbackLines ?? 1000, focused: false }
  root = React.createRef<HTMLDivElement>()
  input = React.createRef<HTMLTextAreaElement>()
  observer?: ResizeObserver
  lastSize?: { cols: number; rows: number }
  handle: TerminalSurfaceHandle = { focus: () => this.input.current?.focus(), copySelection: () => this.copySelection(), clearSelection: () => this.clearSelection(), measure: () => this.measure() }
  static getDerivedStateFromProps(props: Props, state: State): Partial<State> | null {
    const limit = props.scrollbackLines ?? 1000
    if (props.screen === state.previous && limit === state.limit) return null
    return { previous: props.screen, limit, history: appendLocalHistory(state.previous, props.screen, state.history, limit) }
  }
  setHandle(ref: Props['handleRef'], value: TerminalSurfaceHandle | null) { if (typeof ref === 'function') ref(value); else if (ref) ref.current = value }
  componentDidMount() {
    this.setHandle(this.props.handleRef, this.handle)
    this.observer = new ResizeObserver(() => this.reportSize())
    if (this.root.current) this.observer.observe(this.root.current)
    this.reportSize()
  }
  componentDidUpdate(previous: Props) {
    if (previous.handleRef !== this.props.handleRef) { this.setHandle(previous.handleRef, null); this.setHandle(this.props.handleRef, this.handle) }
    if (previous.font !== this.props.font || previous.onResize !== this.props.onResize || previous.connection.state !== this.props.connection.state) this.reportSize()
  }
  componentWillUnmount() { this.observer?.disconnect(); this.setHandle(this.props.handleRef, null) }
  measure() {
    const root = this.root.current
    if (!root) return
    const font = this.props.font ?? defaultFont
    return { cols: Math.max(1, Math.floor(root.clientWidth / (font.sizePx * (font.advanceEm ?? 0.6)))), rows: Math.max(1, Math.floor(root.clientHeight / font.lineHeightPx)) }
  }
  reportSize() {
    const size = this.measure()
    if (!size || this.props.connection.state !== 'live' || (size.cols === this.lastSize?.cols && size.rows === this.lastSize?.rows)) return
    this.lastSize = size
    this.props.onResize?.(size)
  }
  editable() { return !this.props.readOnly && this.props.connection.state === 'live' && this.props.screen !== null }
  clearSelection() {
    const selection = this.root.current?.ownerDocument.getSelection()
    if (selection && this.root.current?.contains(selection.anchorNode)) selection.removeAllRanges()
  }
  selectionText() {
    const root = this.root.current
    const selection = root?.ownerDocument.getSelection()
    if (!root || !selection || selection.isCollapsed || !root.contains(selection.anchorNode) || !root.contains(selection.focusNode)) return
    const range = selection.getRangeAt(0)
    let result = ''
    let previousWrapped = false
    let first = true
    for (const line of root.querySelectorAll<HTMLElement>('[data-terminal-line]')) {
      if (!range.intersectsNode(line)) continue
      const part = range.cloneRange()
      if (!line.contains(part.startContainer)) part.setStart(line, 0)
      if (!line.contains(part.endContainer)) part.setEnd(line, line.childNodes.length)
      const text = part.toString()
      if (!text) continue
      result += (!first && !previousWrapped ? '\n' : '') + text
      first = false
      previousWrapped = line.dataset.wrapped === 'true'
    }
    return result || undefined
  }
  copySelection() { const text = this.selectionText(); if (text !== undefined) this.props.onCopy?.(text); return text }
  send(data: string) { if (!this.editable()) return; this.clearSelection(); this.props.onInput?.(data) }
  focus(focused: boolean) {
    this.setState({ focused })
    this.props.onFocusChange?.(focused)
    if (this.editable() && this.props.screen?.modes.focus_events) this.props.onInput?.(focused ? '\x1b[I' : '\x1b[O')
  }
  render() {
    const { screen, connection, palette, readOnly, readOnlyReason, label = 'Interactive terminal', font = defaultFont } = this.props
    const reason = readOnlyReason || 'Read-only terminal. You do not have permission to send input.'
    const lines = [...this.state.history.lines, ...(screen?.lines ?? [])]
    return <section aria-label={label} {...stylex.props(styles.surface)}>
      {(readOnly || connection.state !== 'live' || !screen) && <div role="status" {...stylex.props(styles.notice)}>{readOnly && connection.state === 'live' ? reason : terminalConnectionReason(connection)}{(connection.state === 'ended' || connection.state === 'unavailable' || connection.state === 'reconnecting') && this.props.onRecover && <Button onPress={this.props.onRecover} {...stylex.props(styles.action)}>{this.props.recoveryLabel || (connection.state === 'ended' ? 'Start a new terminal' : 'Reconnect')}</Button>}</div>}
      <div role="note" {...stylex.props(styles.history)}>Local scrollback · {this.state.history.lines.length} lines{this.state.history.truncated ? ' · older local lines discarded' : ''}{screen?.truncated ? ' · projected output truncated' : ''}</div>
      <div ref={this.root} data-testid="terminal-surface" data-local-lines={this.state.history.lines.length} data-history-truncated={this.state.history.truncated} {...this.props.focusProps}
        {...stylex.props(styles.viewport, this.props.visibleFocus && styles.focus)}
        style={{ fontFamily: font.family, fontSize: font.sizePx, lineHeight: `${font.lineHeightPx}px`, color: palette.foreground, backgroundColor: palette.background, '--terminal-selection': palette.selection } as React.CSSProperties}
        onFocus={event => { this.props.focusProps.onFocus?.(event); if (!event.currentTarget.contains(event.relatedTarget)) this.focus(true) }}
        onBlur={event => { this.props.focusProps.onBlur?.(event); if (!event.currentTarget.contains(event.relatedTarget)) this.focus(false) }}
        onKeyDown={event => {
          if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'c' && this.selectionText()) return
          if (!screen || !this.editable()) return
          const value = encodeTerminalKey(event.nativeEvent, screen.modes)
          if (value !== undefined) { event.preventDefault(); this.send(value) }
        }}
        onPaste={event => { event.preventDefault(); if (!this.editable() || !screen) return; const text = event.clipboardData.getData('text/plain'); this.props.onPaste?.(text); this.send(encodeTerminalPaste(text, screen.modes.bracketed_paste)) }}
        onCopy={event => { const text = this.copySelection(); if (text !== undefined) { event.preventDefault(); event.clipboardData.setData('text/plain', text) } }}>
        <textarea ref={this.input} aria-label={label} aria-readonly={!this.editable()} aria-description={readOnly ? reason : 'Terminal input. Shift+Tab leaves this surface.'} readOnly={!this.editable()} spellCheck={false} autoCapitalize="off" autoCorrect="off" {...stylex.props(styles.input)}
          onChange={event => { if (!event.nativeEvent.isTrusted || !(event.nativeEvent as InputEvent).isComposing) { this.send(event.currentTarget.value); event.currentTarget.value = '' } }}
          onCompositionEnd={event => { this.send(event.data); event.currentTarget.value = '' }} />
        <div data-terminal-output {...stylex.props(styles.output)}>
          {!screen && <div {...stylex.props(styles.placeholder)}>{terminalConnectionReason(connection)}</div>}
          {lines.map((line, index) => <div key={index} data-terminal-line data-wrapped={line.wrapped === true} data-row={line.row} style={{ height: font.lineHeightPx }} {...stylex.props(styles.line)}>{line.runs.map((run, runIndex) => {
            let fg = run.fg === undefined ? palette.foreground : palette.resolve(run.fg)
            let bg = run.bg === undefined ? palette.background : palette.resolve(run.bg)
            if (run.inverse) [fg, bg] = [bg, fg]
            const paint = { color: fg, backgroundColor: bg, opacity: run.dim ? 0.6 : 1, fontWeight: run.bold ? 700 : 400, fontStyle: run.italic ? 'italic' : 'normal', textDecoration: [run.underline && 'underline', run.strikethrough && 'line-through'].filter(Boolean).join(' ') || 'none', ...(run.cells === undefined ? {} : { display: 'inline-block', width: `${run.cells * font.sizePx * (font.advanceEm ?? 0.6)}px` }) }
            const safeLink = run.link && /^(https?:|mailto:)/i.test(run.link.uri)
            return safeLink ? <a key={runIndex} href={run.link!.uri} target="_blank" rel="noreferrer" style={paint} {...stylex.props(styles.link)}>{run.text}</a> : <span key={runIndex} style={paint}>{run.text}</span>
          })}</div>)}
          {screen?.cursor.visible && this.state.focused && <span aria-hidden="true" data-terminal-cursor={screen.cursor.style} {...stylex.props(styles.cursor, screen.cursor.blinking && styles.blink)} style={{ color: palette.cursor, backgroundColor: screen.cursor.style === 'block' ? palette.cursor : 'transparent', width: screen.cursor.style === 'bar' ? 2 : font.sizePx * (font.advanceEm ?? 0.6), height: screen.cursor.style === 'underline' ? 2 : font.lineHeightPx, top: (this.state.history.lines.length + screen.cursor.row) * font.lineHeightPx + (screen.cursor.style === 'underline' ? font.lineHeightPx - 2 : 0), left: screen.cursor.column * font.sizePx * (font.advanceEm ?? 0.6) }} />}
        </div>
      </div>
    </section>
  }
}
const styles = stylex.create({
  surface: { display: 'flex', flexDirection: 'column', flex: 1, minHeight: 0, minWidth: 0, backgroundColor: surface.terminal },
  notice: { padding: 12, color: c.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, display: 'flex', alignItems: 'center', gap: 12 },
  history: { paddingBlock: 4, paddingInline: 12, color: c.fgFaint, fontFamily: t.fontSans, fontSize: t.metaSize, flexShrink: 0 },
  viewport: { flex: 1, minHeight: 0, minWidth: 0, overflow: 'auto', position: 'relative', outline: 'none', margin: 4, '::selection': { backgroundColor: 'var(--terminal-selection)' } },
  focus: { outline: `2px solid ${c.primary}`, outlineOffset: 1 },
  input: { position: 'absolute', width: 1, height: 1, opacity: 0, padding: 0, borderWidth: 0, insetInlineStart: 0, insetBlockStart: 0, resize: 'none' },
  output: { position: 'relative', width: 'max-content', minWidth: '100%', minHeight: '100%', whiteSpace: 'pre' },
  line: { '::selection': { backgroundColor: 'var(--terminal-selection)' } },
  placeholder: { padding: 12, color: c.fgMuted },
  link: { ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 1 } },
  cursor: { position: 'absolute', pointerEvents: 'none', borderBottom: '2px solid currentColor', opacity: 0.65 },
  blink: { animationName: blink, animationDuration: '1.2s', animationIterationCount: 'infinite', '@media (prefers-reduced-motion: reduce)': { animationName: 'none' } },
  action: { backgroundColor: c.controlFill, color: c.fg, border: `1px solid ${c.borderStrong}`, borderRadius: 6, padding: 6, cursor: 'pointer', ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 1 } },
})

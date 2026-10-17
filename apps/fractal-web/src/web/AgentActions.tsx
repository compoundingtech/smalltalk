import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import * as Aria from 'react-aria-components'

import { colorVars as c, spaceVars as s, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import type { AgentControl, AgentControlResult } from '../agents/agent-control-port.ts'
import { useDataSource } from '../data/react.tsx'
import type { Agent } from '../data/source.ts'

// An agent's actions, as stui's agent actions menu offers them (crates/stui/src/ui/screens.rs,
// `agent_actions`): what can be done to it now, in the same order and words. What changes the
// seat asks to confirm first; starting or resuming does not. Interrupt only stops the current turn,
// as the composer's Stop and Esc do, so it does not ask either.

export type AgentActionKey = 'interrupt' | AgentControl | 'copy'

export type AgentActionRow = {
  readonly key: AgentActionKey
  readonly label: string
  readonly what: string
  /** Changes the seat, so it asks first. */
  readonly confirm: boolean
}

/** st states in which a seat has (or is getting) a process, as stui reads them. */
const RUNNING = new Set(['running', 'waiting', 'starting', 'desired'])

export const agentActions = (agent: { readonly state?: string; readonly activity?: Agent['activity'] }): readonly AgentActionRow[] => {
  const running = agent.state !== undefined && RUNNING.has(agent.state)
  return [
    ...(running && agent.activity === 'working'
      ? [{ key: 'interrupt', label: 'Interrupt', what: 'stop its current turn, as Esc does in its own screen', confirm: false } as const]
      : []),
    ...(running
      ? ([
          { key: 'restart', label: 'Restart', what: 'a new process for the same seat, on its declaration', confirm: true },
          { key: 'suspend', label: 'Suspend', what: 'stop at a quiet moment, keeping its session to resume', confirm: true },
          { key: 'retire', label: 'Retire', what: 'stop it and take it off the lists; start brings it back', confirm: true },
        ] as const)
      : ([
          { key: 'start', label: 'Start', what: 'start it again on its declaration', confirm: false },
          { key: 'resume', label: 'Resume', what: 'resume a suspended seat on the same session', confirm: false },
        ] as const)),
    { key: 'copy', label: 'Copy id', what: 'copy its id to the clipboard', confirm: false },
  ]
}

export const AgentActions = ({ agent }: { readonly agent: Pick<Agent, 'ref' | 'name' | 'state' | 'activity'> }): React.ReactNode => {
  const port = useDataSource().agentControls
  const rows = agentActions(agent)
  const [confirming, setConfirming] = React.useState<AgentActionRow>()
  const [status, setStatus] = React.useState<string>()
  const [busy, setBusy] = React.useState(false)
  const run = (row: AgentActionRow) => {
    setConfirming(undefined)
    if (row.key === 'copy') {
      void navigator.clipboard?.writeText(agent.ref).then(() => setStatus(`Copied ${agent.ref}`), () => setStatus(`Could not copy ${agent.ref}`))
      return
    }
    if (port === undefined) {
      setStatus('This connection cannot change agents.')
      return
    }
    setBusy(true)
    setStatus(`${row.label} ${agent.name}…`)
    const sent: Promise<AgentControlResult> = row.key === 'interrupt' ? port.interrupt(agent.ref) : port.control(agent.ref, row.key)
    void sent.then((result) => {
      setBusy(false)
      setStatus(result._tag === 'Sent' ? `${row.label} ${agent.name}: asked st` : result.detail)
    })
  }
  return (
    <div {...stylex.props(styles.actions)}>
      {status ? <span role="status" {...stylex.props(styles.status)}>{status}</span> : null}
      <Aria.MenuTrigger>
        <Aria.Button aria-label={`Actions for ${agent.name}`} isDisabled={busy} {...stylex.props(styles.button)}>Agent</Aria.Button>
        <Aria.Popover placement="bottom end" {...stylex.props(styles.popover)}>
          <Aria.Menu
            aria-label={`Actions for ${agent.name}`}
            onAction={(key) => {
              const row = rows.find((each) => each.key === key)
              if (row === undefined) return
              if (row.confirm) setConfirming(row)
              else run(row)
            }}
            {...stylex.props(styles.menu)}
          >
            {rows.map((row) => (
              <Aria.MenuItem key={row.key} id={row.key} textValue={row.label} {...stylex.props(styles.menuItem)}>
                <span {...stylex.props(styles.label)}>{row.label}</span>
                <span {...stylex.props(styles.what)}>{row.what}</span>
              </Aria.MenuItem>
            ))}
          </Aria.Menu>
        </Aria.Popover>
      </Aria.MenuTrigger>
      <Aria.ModalOverlay isDismissable isOpen={confirming !== undefined} onOpenChange={(open) => { if (!open) setConfirming(undefined) }} {...stylex.props(styles.overlay)}>
        <Aria.Modal {...stylex.props(styles.modal)}>
          <Aria.Dialog role="alertdialog" aria-label={confirming === undefined ? 'Confirm' : `${confirming.label} ${agent.name}?`} {...stylex.props(styles.dialog)}>
            {confirming === undefined ? null : (
              <>
                <strong>{confirming.label} {agent.name}?</strong>
                <p {...stylex.props(styles.what)}>It will {confirming.what}.</p>
                <div {...stylex.props(styles.choices)}>
                  <Aria.Button autoFocus onPress={() => setConfirming(undefined)} {...stylex.props(styles.button, styles.bordered)}>Keep it</Aria.Button>
                  <Aria.Button onPress={() => run(confirming)} {...stylex.props(styles.button, styles.danger)}>{confirming.label}</Aria.Button>
                </div>
              </>
            )}
          </Aria.Dialog>
        </Aria.Modal>
      </Aria.ModalOverlay>
    </div>
  )
}

const styles = stylex.create({
  actions: { display: 'flex', alignItems: 'center', gap: s.md },
  status: { fontSize: t.denseSize, color: c.fgMuted, maxWidth: 280, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  button: { display: 'inline-flex', alignItems: 'center', gap: s.xs, padding: '4px 8px', borderWidth: 0, borderRadius: 6, backgroundColor: 'transparent', color: c.fgMuted, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: c.rowHover }, ':focus-visible': { outline: `2px solid ${c.primary}` }, ':disabled': { opacity: 0.5, cursor: 'default' } },
  bordered: { borderWidth: 1, borderStyle: 'solid', borderColor: c.borderStrong, color: c.fg },
  danger: { borderWidth: 1, borderStyle: 'solid', borderColor: c.dangerFg, color: c.dangerFg },
  popover: { backgroundColor: c.raised, color: c.fg, border: `1px solid ${c.border}`, borderRadius: 6, fontFamily: t.fontSans, fontSize: t.metaSize, zIndex: 100 },
  menu: { padding: s.xs, outline: 'none', maxWidth: 360 },
  menuItem: { display: 'flex', flexDirection: 'column', gap: 2, padding: s.md, outline: 'none', borderRadius: 4, cursor: 'pointer', ':focus': { backgroundColor: c.rowHover }, ':is([data-focus-visible])': { outlineWidth: 2, outlineStyle: 'solid', outlineColor: c.fgMuted, outlineOffset: -2 } },
  label: { color: c.fg },
  what: { margin: 0, color: c.fgMuted, fontSize: t.denseSize },
  overlay: { position: 'fixed', inset: 0, zIndex: 100, display: 'flex', alignItems: 'center', justifyContent: 'center', backgroundColor: 'rgba(0, 0, 0, 0.4)' },
  modal: { width: 360, maxWidth: 'calc(100vw - 32px)', backgroundColor: c.raised, color: c.fg, border: `1px solid ${c.border}`, borderRadius: 8, fontFamily: t.fontSans, fontSize: t.metaSize },
  dialog: { display: 'flex', flexDirection: 'column', gap: s.md, padding: s.lg, outline: 'none' },
  choices: { display: 'flex', justifyContent: 'flex-end', gap: s.md },
})

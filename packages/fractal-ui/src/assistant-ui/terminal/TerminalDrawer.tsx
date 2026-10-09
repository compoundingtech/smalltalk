import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Dialog, Heading, Modal, ModalOverlay } from 'react-aria-components'
import { colorVars as c, typeVars as t, surfaceVars as surface } from '../composition-tokens.stylex'
import { ResizableSplit } from '../composition/Shell'
import { TerminalSurface } from './TerminalSurface'
import { ThemePortal } from '../taste/ThemePortal'
import type { TerminalDrawerProps } from './terminal-types'
export function TerminalDrawer({ open, height, onHeight, onToggle, onReset, min = 120, max = 600, onDetach, onKill, onAdd, label = 'Terminal', ...surface }: TerminalDrawerProps) {
  const [confirmKill, setConfirmKill] = React.useState(false)
  const [previewHeight, setPreviewHeight] = React.useState<number>()
  const closeDrawer = () => {
    setConfirmKill(false)
    setPreviewHeight(undefined)
    onDetach?.()
    onToggle?.()
  }
  if (!open) return null
  return <section aria-label="Terminal drawer" data-testid="terminal-drawer" style={{ height: previewHeight ?? height }} {...stylex.props(styles.drawer)}>
    <ResizableSplit id="terminal-drawer" value={previewHeight ?? height} min={min} max={max} onChange={value => setPreviewHeight(value === height ? undefined : value)} onCommit={value => { setPreviewHeight(undefined); onHeight(value) }} onToggle={onToggle || onDetach ? closeDrawer : () => onHeight(min)} onReset={() => { setPreviewHeight(undefined); if (onReset) onReset(); else onHeight(Math.min(max, Math.max(min, 220))) }} collapsed={false} label="Terminal drawer height" orientation="horizontal" reverse />
    <header {...stylex.props(styles.header)}><strong {...stylex.props(styles.title)}>{surface.screen?.title || label}</strong><span {...stylex.props(styles.meta)}>{surface.screen ? `${surface.screen.columns} × ${surface.screen.rows}` : 'No terminal output yet'}</span>
      {onAdd && surface.connection.state === 'ended' && <Button onPress={onAdd} {...stylex.props(styles.button)}>New terminal</Button>}
      {onKill && <Button onPress={() => setConfirmKill(true)} {...stylex.props(styles.button, styles.danger)}>End terminal</Button>}
      {(onToggle || onDetach) && <Button aria-label="Close terminal drawer" onPress={closeDrawer} {...stylex.props(styles.button)}>Close</Button>}
    </header>
    <TerminalSurface {...surface} label={label} />
    <ThemePortal><ModalOverlay isOpen={confirmKill} onOpenChange={setConfirmKill} isDismissable {...stylex.props(styles.overlay)}><Modal {...stylex.props(styles.modal)}><Dialog role="alertdialog" aria-describedby="terminal-end-reason" {...stylex.props(styles.dialog)}>
      <Heading slot="title">End this terminal?</Heading><p id="terminal-end-reason">This stops the terminal process. Closing the drawer only detaches this view and leaves the process running.</p>
      <div {...stylex.props(styles.actions)}><Button onPress={() => setConfirmKill(false)} {...stylex.props(styles.button)}>Cancel</Button><Button onPress={() => { onKill?.(); setConfirmKill(false) }} {...stylex.props(styles.button, styles.danger)}>End terminal process</Button></div>
    </Dialog></Modal></ModalOverlay></ThemePortal>
  </section>
}
const styles = stylex.create({
  drawer: { display: 'flex', flexDirection: 'column', minHeight: 0, minWidth: 0, flexShrink: 0, borderTopWidth: 1, borderTopStyle: 'solid', borderTopColor: c.borderStrong, backgroundColor: surface.terminal },
  header: { display: 'flex', alignItems: 'center', gap: 12, paddingInline: 12, height: 40, flexShrink: 0, color: c.fg, fontFamily: t.fontSans, fontSize: t.metaSize },
  title: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontWeight: 500 },
  meta: { color: c.fgMuted, marginInlineEnd: 'auto' },
  button: { paddingBlock: 4, paddingInline: 8, borderWidth: 1, borderStyle: 'solid', borderColor: c.borderStrong, borderRadius: 6, color: c.fg, backgroundColor: c.controlFill, cursor: 'pointer', fontSize: t.metaSize, ':hover': { backgroundColor: c.rowHover }, ':focus-visible': { outlineWidth: 2, outlineStyle: 'solid', outlineColor: c.primary, outlineOffset: 1 } },
  danger: { color: c.danger, borderColor: c.dangerMuted },
  overlay: { position: 'fixed', inset: 0, zIndex: 100, backgroundColor: surface.scrim, display: 'flex', alignItems: 'center', justifyContent: 'center' },
  modal: { width: 'min(448px, calc(100vw - 32px))', backgroundColor: c.raised, color: c.fg, borderWidth: 1, borderStyle: 'solid', borderColor: c.borderStrong, borderRadius: 10, fontFamily: t.fontSans },
  dialog: { padding: 20, outlineStyle: 'none', fontSize: t.uiSize },
  actions: { display: 'flex', justifyContent: 'flex-end', gap: 8 },
})

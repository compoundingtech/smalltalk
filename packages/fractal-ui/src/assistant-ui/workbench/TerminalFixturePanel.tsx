import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Tabs, TabList, Tab, TabPanel, Button, DialogTrigger, ModalOverlay, Modal, Dialog, Heading } from 'react-aria-components'
import type { TerminalFrame } from './terminal-model'
import { terminalCapabilityReason } from './terminal-model'
import { surfaceVars as surface, textVars as text, borderVars as border, accentVars as accent, statusVars as status, spaceVars as s, typeVars as t, geometryVars as g, radiusVars as r } from '../composition-tokens.stylex'

/** Story-only display of an observed terminal frame. No PTY or live connection is claimed. */
export function TerminalFixturePanel({ frame, selectedRef, onSelect, onHide, onAdd, onKill }: {
  readonly frame: TerminalFrame
  readonly selectedRef?: string
  readonly onSelect: (ref: string) => void
  readonly onHide: () => void
  readonly onAdd?: () => void
  readonly onKill?: (ref: string) => void
}) {
  const selected = frame.state === 'supported' ? frame.sessions.find(session => session.ref === selectedRef) ?? frame.sessions[0] : undefined
  const titleId = React.useId()
  return <section aria-label="Terminal fixture snapshots" data-testid="terminal-drawer" {...stylex.props(styles.root)}>
    <header {...stylex.props(styles.header)}><strong>Terminal · observed fixture</strong><Button aria-label="Hide terminal" onPress={onHide} {...stylex.props(styles.button)}>Hide</Button></header>
    {frame.state === 'unknown' ? <p {...stylex.props(styles.note)}>Terminal support has not been reported.</p> : frame.state === 'unsupported' ? <p {...stylex.props(styles.note)}>{frame.reason}</p> : <>
      <div {...stylex.props(styles.actions)}><Button isDisabled={frame.add.state !== 'supported' || onAdd === undefined} onPress={onAdd} {...stylex.props(styles.button)}>Add terminal</Button>{terminalCapabilityReason(frame.add, 'Add terminal') && <span>{terminalCapabilityReason(frame.add, 'Add terminal')}</span>}
        {selected && <DialogTrigger><Button aria-label={`Kill session: ${selected.title} terminal`} isDisabled={frame.kill.state !== 'supported' || selected.status !== 'running' || onKill === undefined} {...stylex.props(styles.button)}>Kill session</Button><ModalOverlay isDismissable {...stylex.props(styles.overlay)}><Modal {...stylex.props(styles.modal)}><Dialog role="alertdialog" aria-labelledby={titleId} {...stylex.props(styles.dialog)}>{({ close }) => <><Heading id={titleId} slot="title">Kill session?</Heading><p>The fixture session will be marked exited. Its observed output stays available.</p><div {...stylex.props(styles.actions)}><Button autoFocus onPress={close} {...stylex.props(styles.button)}>Cancel</Button><Button onPress={() => { onKill?.(selected.ref); close() }} {...stylex.props(styles.button)}>Kill session</Button></div></>}</Dialog></Modal></ModalOverlay></DialogTrigger>}
      </div>
      {selected === undefined ? <p {...stylex.props(styles.note)}>No terminal sessions have been supplied.</p> : <Tabs selectedKey={selected.ref} onSelectionChange={key => onSelect(String(key))} {...stylex.props(styles.tabs)}>
        <TabList aria-label="Terminal sessions" {...stylex.props(styles.tabList)}>{frame.sessions.map(session => <Tab key={session.ref} id={session.ref} {...stylex.props(styles.tab, session.ref === selected.ref && styles.tabOn)}>{session.title}</Tab>)}</TabList>
        {/* Every tab controls an existing panel; inactive snapshots remain mounted but hidden. */}
        {frame.sessions.map(session => <TabPanel key={session.ref} id={session.ref} shouldForceMount {...stylex.props(styles.body, session.ref !== selected.ref && styles.hiddenPanel)}>
          <p {...stylex.props(styles.note)}>{session.cwd} · {session.command} · {session.status}</p>
          <pre {...stylex.props(styles.output)}>{session.output.map((line, index) => <span key={index} {...stylex.props(line.tone === 'error' && styles.error)}>{line.text}{line.cursor && <span aria-label="Observed cursor">▌</span>}{'\n'}</span>)}</pre>
        </TabPanel>)}
      </Tabs>}
    </>}
  </section>
}
const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', flexShrink: 0, height: g.drawerDefault, minHeight: 0, backgroundColor: surface.canvas, color: text.fg, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.borderStrong, fontFamily: t.fontSans, fontSize: t.metaSize },
  header: { display: 'flex', alignItems: 'center', justifyContent: 'space-between', paddingInline: s.lg, minHeight: g.controlLg }, actions: { display: 'flex', alignItems: 'center', gap: s.sm, paddingInline: s.lg },
  button: { minHeight: g.controlMd, paddingInline: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, backgroundColor: surface.controlFill, color: text.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':focus': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  tabs: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minHeight: 0 }, tabList: { display: 'flex', gap: s.sm, paddingInline: s.lg }, tab: { padding: s.sm, color: text.fgMuted, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } }, tabOn: { color: text.fg, backgroundColor: surface.rowActive },
  body: { minHeight: 0, overflow: 'auto', paddingInline: s.lg, outlineStyle: 'none' }, note: { marginBlock: s.xs, color: text.fgMuted }, output: { margin: 0, fontFamily: t.fontMono, fontSize: t.codeSize, lineHeight: t.uiLeading }, error: { color: status.dangerFg },
  hiddenPanel: { display: 'none' },
  overlay: { position: 'fixed', inset: 0, zIndex: 30, display: 'flex', alignItems: 'center', justifyContent: 'center', backgroundColor: surface.scrim }, modal: { width: g.modalMax, maxWidth: `calc(100% - ${s.lg})`, backgroundColor: surface.canvas, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, color: text.fg }, dialog: { padding: s.lg, fontFamily: t.fontSans, fontSize: t.uiSize },
})

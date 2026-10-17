// Workshop-only composition: one React Aria dialog with virtual-focus autocomplete; no second command registry.
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Autocomplete, Dialog, Header, Input, Keyboard, Label, Menu, MenuItem, MenuSection, Modal, ModalOverlay, SearchField } from 'react-aria-components'
import { SidebarStatus, type DiscRefinement } from '../../assistant-ui/sidebar/SidebarStatus'
import type { SidebarAgentRow as Row } from '../../assistant-ui/sidebar/model'
import { Icon } from '../../assistant-ui/composition/Icons'
import { accentVars as accent, borderVars as border, elevationVars as elevation, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../../assistant-ui/composition-tokens.stylex'

export interface PaletteCommand { readonly id: string; readonly label: string; readonly shortcut?: string; readonly run: () => void }
export interface PaletteAgent { readonly row: Row; readonly folder: string }
export interface MilestonePaletteProps {
  readonly isOpen: boolean
  readonly onOpenChange: (open: boolean) => void
  readonly scope: 'mixed' | 'agents'
  readonly agents: readonly PaletteAgent[]
  readonly commands: readonly PaletteCommand[]
  readonly glyphRefinement: DiscRefinement
  readonly onOpenAgent: (row: Row) => void
  readonly initialQuery?: string
}

export function MilestonePalette({ isOpen, onOpenChange, scope, agents, commands, glyphRefinement, onOpenAgent, initialQuery = '' }: MilestonePaletteProps) {
  const [query, setQuery] = React.useState(initialQuery)
  // In agents scope a leading ">" switches the list to commands, as in most editors.
  const commandMode = scope === 'agents' && query.startsWith('>')
  const needle = (commandMode ? query.slice(1) : query).trim().toLowerCase()
  const shownAgents = commandMode ? [] : agents.filter(({ row, folder }) => `${row.title} ${folder} ${row.host}`.toLowerCase().includes(needle))
  const shownCommands = scope === 'agents' && !commandMode ? [] : commands.filter(command => command.label.toLowerCase().includes(needle))
  const close = () => { onOpenChange(false); setQuery('') }
  const onAction = (key: React.Key) => {
    const agent = agents.find(candidate => candidate.row.ref === key)
    if (agent !== undefined) onOpenAgent(agent.row)
    else commands.find(command => command.id === key)?.run()
    close()
  }
  return <ModalOverlay isOpen={isOpen} onOpenChange={open => { if (open) onOpenChange(true); else close() }} isDismissable {...stylex.props(styles.overlay)}>
    <Modal {...stylex.props(styles.modal)}>
      <Dialog aria-label="Command palette" data-palette-scope={scope} {...stylex.props(styles.dialog)}>
        <Autocomplete inputValue={query} onInputChange={setQuery}>
          <SearchField autoFocus {...stylex.props(styles.search)}>
            <Label {...stylex.props(styles.visuallyHidden)}>{scope === 'agents' ? 'Open an agent, or type > for commands' : 'Open an agent or run a command'}</Label>
            <Icon name="search" size={14} />
            <Input placeholder={scope === 'agents' ? 'Open an agent…  (> for commands)' : 'Open an agent or run a command…'} {...stylex.props(styles.input)} />
          </SearchField>
          {/* Virtual focus starts on the first result on open and after every query change, so Enter always opens the highlighted row.
              Remounting the menu per query re-applies autoFocus; the menu itself scrolls, so no wrapper takes a Tab stop. */}
          <Menu key={needle} autoFocus="first" aria-label="Results" onAction={onAction} renderEmptyState={() => <p {...stylex.props(styles.empty)}>No matches for “{needle}”</p>} {...stylex.props(styles.menu)}>
            {shownAgents.length > 0 && <MenuSection>
              <Header {...stylex.props(styles.section)}>Agents</Header>
              {shownAgents.map(({ row, folder }) => <MenuItem key={row.ref} id={row.ref} textValue={row.title} {...stylex.props(styles.item)}>
                <SidebarStatus status={row.status} statusLabel={row.statusLabel} variant="SG-1" discRefinement={glyphRefinement} freshness={row.freshness} iconOnly />
                <span {...stylex.props(styles.label)}>{row.title}</span>
                <span {...stylex.props(styles.meta)}>{row.parentRef === undefined ? folder : 'subagent'}</span>
              </MenuItem>)}
            </MenuSection>}
            {shownCommands.length > 0 && <MenuSection>
              <Header {...stylex.props(styles.section)}>Commands</Header>
              {shownCommands.map(command => <MenuItem key={command.id} id={command.id} textValue={command.label} {...stylex.props(styles.item)}>
                <span aria-hidden="true" {...stylex.props(styles.glyphSlot)}><Icon name="chevron-right" size={12} /></span>
                <span {...stylex.props(styles.label)}>{command.label}</span>
                {command.shortcut !== undefined && <Keyboard {...stylex.props(styles.kbd)}>{command.shortcut}</Keyboard>}
              </MenuItem>)}
            </MenuSection>}
          </Menu>
        </Autocomplete>
        <p aria-hidden="true" {...stylex.props(styles.footer)}><span>↑↓ move</span><span>↵ open</span><span>esc close</span></p>
      </Dialog>
    </Modal>
  </ModalOverlay>
}

const styles = stylex.create({
  overlay: { position: 'fixed', inset: 0, zIndex: 40, display: 'flex', justifyContent: 'center', alignItems: 'flex-start', paddingBlockStart: '12dvh', paddingInline: s.lg, backgroundColor: surface.scrim },
  modal: { width: '560px', maxWidth: '100%', outline: 'none' },
  dialog: { display: 'flex', flexDirection: 'column', overflow: 'hidden', borderRadius: r.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.raised, color: ink.fg, boxShadow: elevation.dialog, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, outline: 'none' },
  search: { display: 'flex', alignItems: 'center', gap: s.md, height: g.band, paddingInline: s.lg, boxSizing: 'border-box', color: ink.fgMuted, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  input: { flex: '1 1 0', minWidth: 0, padding: 0, borderWidth: 0, backgroundColor: surface.transparent, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading, outline: 'none', '::placeholder': { color: ink.fgMuted }, '::-webkit-search-cancel-button': { display: 'none' } },
  menu: { maxHeight: g.commandMaxHeight, overflowY: 'auto', padding: s.xs, outline: 'none' },
  section: { paddingInline: s.md, paddingBlockStart: s.md, paddingBlockEnd: s.xs, color: ink.fgMuted, fontSize: t.denseSize, fontWeight: t.weightMedium },
  item: { display: 'grid', gridTemplateColumns: `${g.icon} minmax(0, 1fr) auto`, alignItems: 'center', columnGap: s.md, height: g.menuRow, paddingInline: s.md, borderRadius: r.sm, cursor: 'pointer', outline: 'none', ':is([data-focused])': { backgroundColor: surface.rowActive }, ':is([data-focus-visible])': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: '-2px' } },
  glyphSlot: { display: 'inline-flex', justifyContent: 'center', color: ink.fgMuted },
  label: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontSize: t.metaSize, color: ink.fg },
  // The first result is always highlighted, so row meta must clear 4.5:1 on the rowActive wash as well as the dialog surface.
  meta: { color: ink.fgSoft, fontSize: t.denseSize, whiteSpace: 'nowrap' },
  kbd: { fontFamily: t.fontSans, fontSize: t.denseSize, color: ink.fgSoft, paddingInline: s.xs, borderRadius: r.keyboard, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong },
  empty: { margin: 0, paddingInline: s.md, paddingBlock: s.lg, color: ink.fgMuted },
  footer: { display: 'flex', gap: s.lg, margin: 0, height: g.controlMd, alignItems: 'center', paddingInline: s.lg, color: ink.fgMuted, fontSize: t.denseSize, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border },
  visuallyHidden: { position: 'absolute', width: '1px', height: '1px', margin: '-1px', padding: 0, overflow: 'hidden', clip: 'rect(0 0 0 0)', whiteSpace: 'nowrap', borderWidth: 0 },
})

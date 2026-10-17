// Multiple-selection filter menu: checkbox entries with counts; selection semantics belong to the host.
import * as stylex from '@stylexjs/stylex'
import { Button, Header, Menu, MenuItem, MenuSection, MenuTrigger, Popover, VisuallyHidden, type Selection } from 'react-aria-components'
import { Icon } from '../composition/Icons'
import { accentVars as accent, borderVars as border, elevationVars as elevation, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'

/** One checkbox entry; `id` is the host's selection key. */
export interface ShowMenuEntry {
  readonly id: string
  readonly label: string
  readonly count: number
}
export interface ShowMenuGroup {
  readonly id: string
  readonly title: string
  readonly entries: readonly ShowMenuEntry[]
}
export interface ShowMenuProps {
  readonly groups: readonly ShowMenuGroup[]
  readonly selectedKeys: ReadonlySet<string>
  readonly onSelectionChange: (keys: ReadonlySet<string>) => void
  /** Visible trigger text and menu name. */
  readonly label?: string
  /** Theme for the portalled popover: themes on the host's subtree do not reach document.body. */
  readonly popoverTheme?: stylex.StyleXArray<stylex.CompiledStyles>
}

/** Checkbox menu entry for a multiple-selection menu: check, label, right-aligned tabular count. */
export function ShowMenuItem({ id, label, count }: ShowMenuEntry) {
  return <MenuItem id={id} textValue={label} aria-label={`${label}, ${count}`} {...stylex.props(styles.item)}>
    {({ isSelected }) => <>
      <span aria-hidden="true" data-show-check={isSelected ? 'checked' : 'unchecked'} {...stylex.props(styles.check, isSelected && styles.checked)}>{isSelected && <Icon name="check" size={12} />}</span>
      <span {...stylex.props(styles.label)}>{label}</span>
      <span aria-hidden="true" data-show-count="" {...stylex.props(styles.count)}>{count}</span>
    </>}
  </MenuItem>
}

/** Trigger plus multiple-selection menu; it stays open while entries toggle. */
export function ShowMenu({ groups, selectedKeys, onSelectionChange, label = 'Show', popoverTheme }: ShowMenuProps) {
  const active = selectedKeys.size
  const select = (keys: Selection) =>
    onSelectionChange(keys === 'all' ? new Set(groups.flatMap(group => group.entries.map(entry => entry.id))) : new Set([...keys].map(String)))
  return <MenuTrigger>
    <Button data-show-active={active > 0 ? active : undefined} {...stylex.props(styles.trigger, active > 0 && styles.triggerActive)}>
      <span>{label}</span>
      {active > 0 && <><span aria-hidden="true" {...stylex.props(styles.badge)}>{active}</span><VisuallyHidden>, {active} active</VisuallyHidden></>}
      <Icon name="chevron-down" size={12} />
    </Button>
    <Popover placement="bottom end" {...stylex.props(styles.popover, popoverTheme)}>
      <Menu aria-label={label} selectionMode="multiple" selectedKeys={selectedKeys} onSelectionChange={select} {...stylex.props(styles.menu)}>
        {groups.map(group => <MenuSection key={group.id} id={group.id} {...stylex.props(styles.section)}>
          <Header {...stylex.props(styles.header)}>{group.title}</Header>
          {group.entries.map(entry => <ShowMenuItem key={entry.id} {...entry} />)}
        </MenuSection>)}
      </Menu>
    </Popover>
  </MenuTrigger>
}

const styles = stylex.create({
  trigger: { display: 'inline-flex', alignItems: 'center', gap: s.xs, flexShrink: 0, minHeight: g.controlMd, paddingInline: s.sm, boxSizing: 'border-box', borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, backgroundColor: { default: surface.controlFill, ':hover': surface.rowHover }, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, whiteSpace: 'nowrap', cursor: 'pointer', outlineStyle: 'none', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  triggerActive: { color: ink.fg, backgroundColor: surface.rowActive },
  badge: { minWidth: g.icon, height: g.icon, paddingInline: s.xs, boxSizing: 'border-box', display: 'inline-flex', alignItems: 'center', justifyContent: 'center', borderRadius: r.full, backgroundColor: accent.primary, color: accent.onPrimary, fontSize: t.denseSize, fontVariantNumeric: 'tabular-nums' },
  popover: { minWidth: g.menuMin, maxWidth: 'calc(100vw - 16px)', overflowY: 'auto', boxSizing: 'border-box', padding: s.xs, backgroundColor: surface.raised, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, boxShadow: elevation.popover, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  menu: { outlineStyle: 'none' },
  section: { display: 'flex', flexDirection: 'column', ':not(:first-child)': { marginTop: s.xs, paddingTop: s.xs, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border } },
  header: { minHeight: g.controlSm, display: 'flex', alignItems: 'center', paddingInline: s.md, color: ink.fgMuted, fontSize: t.denseSize, fontWeight: t.weightMedium },
  item: { minHeight: g.menuRow, display: 'grid', gridTemplateColumns: `${g.icon} minmax(0, 1fr) auto`, alignItems: 'center', columnGap: s.md, paddingInline: s.md, borderRadius: r.sm, cursor: 'pointer', outlineStyle: 'none', ':is([data-focused])': { backgroundColor: surface.rowActive }, ':is([data-focus-visible])': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  check: { width: g.icon, height: g.icon, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', boxSizing: 'border-box', borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.controlFill },
  checked: { borderColor: accent.primary, backgroundColor: accent.primary, color: accent.onPrimary },
  label: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  count: { minWidth: '3ch', textAlign: 'right', color: ink.fgMuted, fontVariantNumeric: 'tabular-nums' },
})

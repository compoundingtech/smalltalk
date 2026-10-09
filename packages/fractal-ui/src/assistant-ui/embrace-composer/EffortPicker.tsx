import * as stylex from '@stylexjs/stylex'
import { Button, Focusable, Header, Menu, MenuItem, MenuSection, MenuTrigger, Popover, Separator, Text, Tooltip, TooltipTrigger } from 'react-aria-components'
import { Icon } from '../composition/Icons'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { RenderProfiler } from '../perf/RenderProfiler'

/** Conversation-frame control: allowed values belong to the harness, not the UI. */
export type EffortControl =
  | { readonly state: 'supported'; readonly values: readonly [string, ...string[]]; readonly default: string }
  | { readonly state: 'unsupported'; readonly reason: string }
  | { readonly state: 'unknown' }
export const unknownEffort: EffortControl = { state: 'unknown' }
export const messageEffort = (custom: Record<string, unknown> | undefined): string | undefined => {
  const value = custom?.effort
  return typeof value === 'string' ? value : undefined
}
export function EffortPicker({ control, value, pinned, onChange, onPinnedChange, compact = false }: {
  readonly control: EffortControl
  readonly value: string | undefined
  readonly pinned: boolean
  readonly onChange: (value: string) => void
  readonly onPinnedChange: (pinned: boolean) => void
  /** Inline pill mode: icon plus level; the menu keeps the full labels. */
  readonly compact?: boolean
}) {
  if (control.state === 'unknown') return null
  if (control.state === 'unsupported') return <TooltipTrigger delay={150}><Focusable><span role="group" tabIndex={0} aria-label="Effort unsupported reason" {...stylex.props(styles.reasonTrigger)}><Button aria-label="Effort unsupported" isDisabled {...stylex.props(styles.button, styles.disabled, compact && styles.buttonCompact)}><Icon name="clock" size={12} /></Button></span></Focusable><Tooltip {...stylex.props(styles.popup)}>{control.reason}</Tooltip></TooltipTrigger>
  const selected = value !== undefined && control.values.includes(value) ? value : control.default
  return <div {...stylex.props(styles.group)}>
    <MenuTrigger>
      <Button aria-label="Select effort" {...stylex.props(styles.button, compact && styles.buttonCompact)}>
        {compact ? <><Icon name="clock" size={12} /><RenderProfiler id="EffortValue"><span title={`Effort: ${effortLabel(selected)}`}>{effortLabel(selected)}</span></RenderProfiler></> : <><RenderProfiler id="EffortValue"><span>Effort: {effortLabel(selected)}</span></RenderProfiler><Icon name="chevron-down" size={12} /></>}
      </Button>
      <Popover {...stylex.props(styles.popup)}>
        <Menu aria-label="Message effort" onAction={key => { if (key === pinItemId) onPinnedChange(!pinned); else onChange(String(key)) }}>
          <MenuSection selectionMode="single" selectedKeys={[selected]} shouldCloseOnSelect={false}>
            <Header {...stylex.props(styles.header)}>Effort</Header>
            {control.values.map(effort => <MenuItem id={effort} key={effort} textValue={effortLabel(effort)} aria-label={effortLabel(effort)} {...stylex.props(styles.option)}>
              {({ isSelected }) => <>
                <span aria-hidden="true" data-effort-check={isSelected ? 'checked' : 'unchecked'} {...stylex.props(styles.indicator)}>{isSelected && <Icon name="check" size={12} />}</span>
                <span {...stylex.props(styles.label)}>{effortLabel(effort)}</span>
                {effort === control.default && <Text slot="description" {...stylex.props(styles.defaultBadge)}>Default</Text>}
              </>}
            </MenuItem>)}
          </MenuSection>
          <Separator {...stylex.props(styles.separator)} />
          <MenuSection selectionMode="multiple" selectedKeys={pinned ? [pinItemId] : []} shouldCloseOnSelect={false}>
          <MenuItem id={pinItemId} textValue="Keep for next messages" aria-label="Keep for next messages" {...stylex.props(styles.option)}>
            {({ isSelected }) => <>
              <span aria-hidden="true" data-effort-pin={isSelected ? 'checked' : 'unchecked'} {...stylex.props(styles.indicator, styles.checkbox, isSelected && styles.checkboxChecked)}>{isSelected && <Icon name="check" size={12} />}</span>
              <span>Keep for next messages</span>
            </>}
          </MenuItem>
          </MenuSection>
        </Menu>
      </Popover>
    </MenuTrigger>
  </div>
}
const pinItemId = 'keep-effort-for-next-messages'
const effortLabel = (value: string): string => value.replace(/\b\p{L}/gu, letter => letter.toUpperCase())
const styles = stylex.create({
  group: { display: 'inline-flex', alignItems: 'center', gap: s.xs, flexShrink: 0, whiteSpace: 'nowrap' },
  reasonTrigger: { display: 'inline-flex', borderRadius: r.control, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  header: { minHeight: g.controlSm, display: 'flex', alignItems: 'center', paddingInline: s.md, color: ink.fgMuted, fontSize: t.denseSize, fontWeight: t.weightMedium },
  separator: { height: g.hairline, marginBlock: s.sm, marginInline: s.md, backgroundColor: border.border, borderWidth: 0 },
  indicator: { width: g.icon, height: g.icon, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', flexShrink: 0, boxSizing: 'border-box' },
  checkbox: { borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.controlFill },
  checkboxChecked: { borderColor: accent.primary, backgroundColor: accent.primary, color: accent.onPrimary },
  label: { flexGrow: 1 },
  defaultBadge: { paddingInline: s.xs, borderRadius: r.sm, backgroundColor: surface.controlFill, color: ink.fgMuted, fontSize: t.denseSize },
  button: { display: 'inline-flex', alignItems: 'center', gap: s.xs, minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  buttonCompact: { paddingInline: s.xs, gap: s.xs2 },
  disabled: { opacity: 0.64, cursor: 'not-allowed', pointerEvents: 'none' },
  popup: { minWidth: '180px', maxWidth: g.tooltipMax, width: 'max-content', padding: s.sm, backgroundColor: surface.raised, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  option: { minHeight: g.controlLg, display: 'flex', alignItems: 'center', gap: s.sm, whiteSpace: 'nowrap', paddingInline: s.md, borderRadius: r.sm, cursor: 'pointer', outlineStyle: 'none', ':focus': { backgroundColor: surface.rowActive } },
})

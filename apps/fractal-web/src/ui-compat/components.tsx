/** Clean-room adapters authored from application call sites and the original Fractal kit API. */
import * as React from 'react'
import * as Aria from 'react-aria-components'
import * as Kit from '@smalltalk/fractal-ui'
import * as stylex from '@stylexjs/stylex'

export { CheckIcon, CopyIcon, XCircleIcon } from '@smalltalk/fractal-ui'
export { Tabs, TabList, Tab, TabPanel, MenuTrigger, TooltipTrigger } from 'react-aria-components'
export type Language = string
export type PillVariant = 'gray' | 'blue' | 'green' | 'amber' | 'red' | 'purple'
export type BadgeVariant = PillVariant | `${PillVariant}-subtle`
type Style = { stylexStyle?: stylex.StyleXStyles }
type Size = 'xs' | 'sm' | 'md' | 'lg' | 'small' | 'compact'
/** Preserve the application's atomic classes alongside adapter defaults. */
function styled<P extends { className?: unknown; style?: unknown }>(props: P, ...rules: (stylex.StyleXStyles | undefined)[]) {
  const atomic = stylex.props(...rules)
  const caller = props.className
  return { ...props, className: typeof caller === 'function' ? (state: unknown) => [atomic.className, (caller as (state: unknown) => string)(state)].filter(Boolean).join(' ') : [atomic.className, caller].filter(Boolean).join(' '), style: { ...atomic.style, ...(props.style as React.CSSProperties) } } as P
}
const tone = (variant?: string): Kit.Tone => variant?.includes('red') || variant === 'error' || variant === 'danger' ? 'danger' : variant?.includes('amber') || variant === 'warning' ? 'warning' : variant?.includes('green') || variant === 'success' ? 'good' : 'neutral'

export type ButtonProps = Omit<Aria.ButtonProps, 'style'> & Style & { size?: Size; variant?: string; shape?: string; style?: React.CSSProperties; ref?: React.Ref<HTMLButtonElement>; isLoading?: boolean }
export function Button({ size, variant, shape, stylexStyle, isLoading, children, ...props }: ButtonProps) {
  return <Kit.Button {...styled(props, stylexStyle)} size={size === 'md' || size === 'lg' ? 'md' : 'sm'} square={shape === 'square'} variant={variant === 'primary' ? 'primary' : variant === 'error' || variant === 'danger' ? 'danger' : variant === 'tertiary' || variant === 'ghost' || variant === 'quiet' ? 'quiet' : 'secondary'} isDisabled={props.isDisabled || isLoading}>{isLoading ? <Spinner size="sm" /> : children}</Kit.Button>
}
export type BadgeProps = React.HTMLAttributes<HTMLSpanElement> & Style & { variant?: BadgeVariant; size?: Size }
export function Badge({ variant, size: _size, stylexStyle, children, ...props }: BadgeProps) { return <span {...styled(props, stylexStyle)}><Kit.Badge tone={tone(variant)}>{children}</Kit.Badge></span> }
export function Pill({ variant, children }: { variant?: PillVariant; children: React.ReactNode }) { return <Kit.Pill tone={tone(variant)}>{children}</Kit.Pill> }
export type StatusDotProps = React.HTMLAttributes<HTMLSpanElement> & Style & { status?: Kit.DotState; state?: Kit.DotState; size?: Size; label?: string }
export function StatusDot({ status, state, size: _size, stylexStyle, label, ...props }: StatusDotProps) { return <span {...styled(props, stylexStyle)}><Kit.StatusDot state={status ?? state ?? 'queued'} label={label ?? ''} /></span> }
export function Spinner({ size, label, ...props }: React.HTMLAttributes<HTMLSpanElement> & { size?: Size; label?: string }) { return <span {...props}><Kit.Spinner label={label ?? ''} size={size === 'lg' || size === 'md' ? 'md' : 'sm'} /></span> }
export function Kbd({ children, size: _size, ...props }: React.HTMLAttributes<HTMLElement> & { size?: Size }) { return <span {...props}><Kit.Kbd>{children}</Kit.Kbd></span> }
export function Note({ variant, size, filled, fill, title, children, stylexStyle, ...props }: Omit<React.HTMLAttributes<HTMLDivElement>, 'title'> & Style & { variant?: string; size?: Size; filled?: boolean; fill?: boolean; title?: string }) { return <div {...styled(props, stylexStyle)}><Kit.Note tone={tone(variant)} size={size === 'sm' ? 'sm' : 'md'} filled={filled ?? fill} title={title}>{children}</Kit.Note></div> }
export function EmptyState({ title, description, hint, icon, action, children }: { title: string; description?: string; hint?: string; icon?: React.ReactNode; action?: React.ReactNode; children?: React.ReactNode }) { return <div>{icon}<Kit.EmptyState title={title} hint={description ?? hint ?? ''} action={action ?? children} /></div> }
export function Description({ title, children, size: _size }: { title: string; children: React.ReactNode; size?: string }) { return <Kit.Description items={[[title, children]]} /> }
export function Entity({ name, description, meta, avatarFallback }: { name: string; description?: React.ReactNode; meta?: React.ReactNode; avatarFallback?: string; size?: Size }) { return <div {...stylex.props(styles.entity)}><span aria-hidden="true" {...stylex.props(styles.avatar)}>{avatarFallback ?? name.slice(0, 2).toUpperCase()}</span><div><strong>{name}</strong>{description ? <div {...stylex.props(styles.detail)}>{description}</div> : null}{meta ? <div {...stylex.props(styles.detail)}>{meta}</div> : null}</div></div> }
export function Progress({ label, value, maxValue, size, color }: { label?: string; value?: number; maxValue?: number; size?: Size; color?: string; 'aria-label'?: string }) { return <Kit.Progress label={label ?? 'Progress'} value={value} maxValue={maxValue} size={size === 'sm' ? 'sm' : 'md'} tone={color === 'orange' ? 'warning' : color === 'red' ? 'danger' : 'neutral'} /> }

export type InputProps = Omit<Aria.InputProps, 'size' | 'onChange' | 'style'> & Style & { label?: string; size?: Size; style?: React.CSSProperties | stylex.StyleXStyles; onChange?: (value: string) => void; isDisabled?: boolean; ref?: React.Ref<HTMLInputElement> }
export function Input({ label, size: _size, onChange, isDisabled, stylexStyle, style, ...props }: InputProps) {
  const atomicStyle = style && '$$css' in style ? style as stylex.StyleXStyles : undefined
  const field = <Aria.Input {...styled({ ...props, style: atomicStyle ? undefined : style as React.CSSProperties }, styles.input, atomicStyle, stylexStyle)} disabled={isDisabled ?? props.disabled} onChange={onChange ? event => onChange(event.currentTarget.value) : undefined} />
  return label === undefined ? field : <Aria.TextField aria-label={label}>{field}</Aria.TextField>
}
export function Checkbox({ label, size: _size, stylexStyle, children, ...props }: Aria.CheckboxProps & Style & { label?: React.ReactNode; size?: Size }) {
  return <Aria.Checkbox {...styled(props, styles.checkbox, stylexStyle)}>{state => <><span {...stylex.props(styles.checkMark, state.isSelected && styles.checked)} aria-hidden="true">{state.isIndeterminate ? '−' : state.isSelected ? '✓' : ''}</span>{typeof children === 'function' ? children(state) : children ?? label}</>}</Aria.Checkbox>
}
export function Toggle({ label, size: _size, ...props }: Aria.SwitchProps & { label?: string; size?: Size }) { return <Aria.Switch {...styled(props, styles.checkbox)}>{state => <><span {...stylex.props(styles.switchTrack, state.isSelected && styles.checked)} aria-hidden="true"><span {...stylex.props(styles.switchThumb, state.isSelected && styles.switchOn)} /></span>{label}</>}</Aria.Switch> }
export function Tooltip({ size: _size, stylexStyle, ...props }: Aria.TooltipProps & Style & { size?: Size }) { return <Aria.Tooltip {...styled(props, styles.tooltip, stylexStyle)} /> }
export const ContextCardTrigger = Aria.TooltipTrigger
export function ContextCard({ size: _size, stylexStyle, ...props }: Aria.TooltipProps & Style & { size?: Size }) { return <Aria.Tooltip {...styled(props, styles.popover, stylexStyle)} /> }
export function MenuPopover({ stylexStyle, ...props }: Aria.PopoverProps & Style) { return <Aria.Popover {...styled(props, styles.popover, stylexStyle)} /> }
export function Menu<T extends object>({ stylexStyle, ...props }: Aria.MenuProps<T> & Style) { return <Aria.Menu {...styled(props, styles.menu, stylexStyle)} /> }
export function MenuItem<T extends object>({ stylexStyle, children, suffix, ...props }: Aria.MenuItemProps<T> & Style & { suffix?: React.ReactNode }) {
  const root = styled(props, styles.menuItem, stylexStyle)
  return <Aria.MenuItem {...root} className={state => [typeof root.className === 'function' ? root.className(state) : root.className, stylex.props(state.isFocused && styles.selected).className].filter(Boolean).join(' ')}>{state => <><span>{typeof children === 'function' ? children(state) : children}</span>{suffix ?? (state.isSelected ? <span aria-hidden="true" style={{ marginLeft: 8 }}>✓</span> : null)}</>}</Aria.MenuItem>
}
export function Table({ size: _size, stylexStyle, ...props }: Aria.TableProps & Style & { size?: Size }) { return <Aria.Table {...styled(props, styles.table, stylexStyle)} /> }
export const TableBody = Aria.TableBody
export const TableHeader = Aria.TableHeader
export function TableColumn({ size: _size, stylexStyle, ...props }: Aria.ColumnProps & Style & { size?: Size }) { return <Aria.Column {...styled(props, styles.cell, styles.column, stylexStyle)} /> }
export function TableRow<T extends object>({ size: _size, stylexStyle, ...props }: Aria.RowProps<T> & Style & { size?: Size }) {
  const root = styled(props, styles.row, stylexStyle)
  return <Aria.Row {...root} className={state => [typeof root.className === 'function' ? root.className(state) : root.className, stylex.props(state.isSelected && styles.selected).className].filter(Boolean).join(' ')} />
}
export function TableCell({ size: _size, stylexStyle, ...props }: Aria.CellProps & Style & { size?: Size }) { return <Aria.Cell {...styled(props, styles.cell, stylexStyle)} /> }
export function Modal({ isDismissable, stylexStyle, ...props }: Omit<Aria.ModalOverlayProps, 'children'> & Style & { children?: React.ReactNode }) { return <Aria.ModalOverlay {...styled(props, styles.overlay, stylexStyle)} isDismissable={isDismissable ?? true}><Aria.Modal {...stylex.props(styles.modal)}>{props.children}</Aria.Modal></Aria.ModalOverlay> }
export function ModalDialog(props: Aria.DialogProps) { return <Aria.Dialog {...styled(props, styles.dialog)} /> }
export function ModalHeader(props: React.HTMLAttributes<HTMLElement>) { return <header {...styled(props, styles.modalHeader)} /> }
export function ModalTitle(props: Aria.HeadingProps) { return <Aria.Heading {...styled(props, styles.modalTitle)} slot="title" /> }
export function ModalDescription(props: Aria.TextProps) { return <Aria.Text {...styled(props, styles.modalDescription)} slot="description" /> }
export function ModalContent(props: React.HTMLAttributes<HTMLDivElement>) { return <div {...styled(props, styles.modalContent)} /> }
export function ModalFooter(props: React.HTMLAttributes<HTMLElement>) { return <footer {...styled(props, styles.modalFooter)} /> }

export interface CommandItem { id: string; label: string; description?: string; keywords?: readonly string[]; kbd?: string; onSelect?: () => void; icon?: React.ReactNode }
export interface CommandGroup { id: string; label: string; items: readonly CommandItem[] }
export function CommandMenu({ groups, isOpen, onOpenChange, isDismissable = true, placeholder, emptyHint }: { groups: readonly CommandGroup[]; isOpen?: boolean; onOpenChange?: (open: boolean) => void; isDismissable?: boolean; placeholder?: string; emptyHint?: string }) {
  const mapped = groups.map(group => ({ ...group, items: group.items.map(item => ({ ...item, onSelect: () => { item.onSelect?.(); onOpenChange?.(false) } })) }))
  const content = <Kit.CommandMenu groups={mapped} placeholder={placeholder} emptyHint={emptyHint} />
  return isOpen === undefined ? content : <Modal isOpen={isOpen} onOpenChange={onOpenChange} isDismissable={isDismissable}><ModalDialog aria-label="Command menu">{content}</ModalDialog></Modal>
}
export function CodeBlock({ code, filename, language = 'text', showCopyButton = true, stylexStyle }: { code: string; filename?: string; language?: Language; showCopyButton?: boolean; stylexStyle?: stylex.StyleXStyles; size?: Size }) {
  const [copyState, setCopyState] = React.useState<'idle' | 'copied' | 'failed'>('idle')
  return <section {...stylex.props(styles.code, stylexStyle)}><header {...stylex.props(styles.codeHeader)}><span>{filename ?? language}</span>{showCopyButton ? <Button size="sm" variant="quiet" aria-label={copyState === 'copied' ? 'Copied' : 'Copy code'} onPress={async () => { try { await navigator.clipboard.writeText(code); setCopyState('copied') } catch { setCopyState('failed') } }}>{copyState === 'copied' ? <Kit.CheckIcon /> : <Kit.CopyIcon />}</Button> : null}</header>{copyState === 'failed' ? <span role="status">Copy failed. Select the code to copy it.</span> : null}<pre {...stylex.props(styles.pre)}><code>{code}</code></pre></section>
}

const styles = stylex.create({
 entity: { display: 'flex', alignItems: 'center', gap: 10 },
 avatar: { minWidth: 28, height: 28, display: 'flex', alignItems: 'center', justifyContent: 'center', backgroundColor: 'var(--selection)', color: 'var(--accent)', borderRadius: 'var(--radius)', fontSize: 11 },
 detail: { color: 'var(--muted)', fontSize: 11, marginTop: 2 },
 input: { width: '100%', minWidth: 0, padding: '6px 8px', color: 'var(--ink)', backgroundColor: 'var(--panel)', borderWidth: 1, borderStyle: 'solid', borderColor: 'var(--line)', borderRadius: 'var(--radius)', fontFamily: 'var(--sans)' },
 checkbox: { display: 'inline-flex', alignItems: 'center', gap: 8, cursor: 'pointer', fontSize: 12, color: 'var(--ink)' },
 checkMark: { width: 16, height: 16, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', borderWidth: 1, borderStyle: 'solid', borderColor: 'var(--line)', borderRadius: 3, backgroundColor: 'var(--panel)' },
 checked: { backgroundColor: 'var(--accent)', color: 'var(--on-accent)', borderColor: 'var(--accent)' },
 switchTrack: { position: 'relative', width: 30, height: 18, borderRadius: 99, backgroundColor: 'var(--recess)', borderWidth: 1, borderStyle: 'solid', borderColor: 'var(--line)' },
 switchThumb: { position: 'absolute', top: 2, left: 2, width: 12, height: 12, borderRadius: 99, backgroundColor: 'var(--panel)' },
 switchOn: { left: 14 },
 tooltip: { zIndex: 100, padding: '6px 9px', borderRadius: 'var(--radius)', color: 'var(--panel)', backgroundColor: 'var(--ink)', fontSize: 12, maxWidth: 360 },
 popover: { zIndex: 100, padding: 6, borderRadius: 'var(--panel-radius)', borderWidth: 1, borderStyle: 'solid', borderColor: 'var(--line)', color: 'var(--ink)', backgroundColor: 'var(--panel)', boxShadow: '0 8px 24px rgb(0 0 0 / .18)', maxWidth: 420 },
 menu: { minWidth: 160, outline: 'none' },
 menuItem: { display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8, padding: '7px 10px', cursor: 'pointer', borderRadius: 'var(--radius)', backgroundColor: { default: 'transparent', ':hover': 'var(--selection)' }, color: 'var(--ink)' },
 selected: { backgroundColor: 'var(--selection)' },
 table: { width: '100%', borderCollapse: 'collapse', fontSize: 12, textAlign: 'left', color: 'var(--ink)' },
 cell: { padding: '6px 9px', borderBottomWidth: 1, borderBottomStyle: 'solid', borderBottomColor: 'var(--line)' },
 column: { color: 'var(--muted)', fontWeight: 500 },
 row: { backgroundColor: { default: 'transparent', ':hover': 'var(--recess)' } },
 overlay: { position: 'fixed', inset: 0, zIndex: 200, backgroundColor: 'rgb(0 0 0 / .45)', display: 'flex', alignItems: 'center', justifyContent: 'center', padding: 24 },
 modal: { width: 'min(640px, 100%)', maxHeight: '90vh', overflow: 'auto', backgroundColor: 'var(--panel)', color: 'var(--ink)', borderRadius: 'var(--panel-radius)', boxShadow: '0 16px 48px rgb(0 0 0 / .25)' },
 dialog: { outline: 'none' },
 modalHeader: { padding: '18px 20px 10px' },
 modalTitle: { fontSize: 18, fontWeight: 600, margin: 0 },
 modalDescription: { display: 'block', fontSize: 12, color: 'var(--muted)', marginTop: 8 },
 modalContent: { padding: '12px 20px' },
 modalFooter: { display: 'flex', justifyContent: 'flex-end', gap: 8, padding: '12px 20px 18px' },
 code: { borderWidth: 1, borderStyle: 'solid', borderColor: 'var(--line)', borderRadius: 'var(--panel-radius)', backgroundColor: 'var(--recess)', overflow: 'hidden' },
 codeHeader: { display: 'flex', justifyContent: 'space-between', alignItems: 'center', padding: '5px 10px', color: 'var(--muted)', fontSize: 11, fontFamily: 'var(--mono)' },
 pre: { padding: 12, margin: 0, overflowX: 'auto', fontFamily: 'var(--mono)', fontSize: 12, color: 'var(--ink)' },
})

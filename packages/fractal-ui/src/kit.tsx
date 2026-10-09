import * as React from 'react'
import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, geometryVars as g, radiusVars as r, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme } from './assistant-ui/composition-theme'

export type Direction = 'folio' | 'relay' | 'orbit'
export type Scheme = 'light' | 'dark'
export type Density = 'compact' | 'comfortable'
export type Tone = 'neutral' | 'good' | 'warning' | 'danger'
export interface Theme { direction: Direction; scheme: Scheme; density: Density }

export function ThemeRoot({ direction, scheme, density, children, className = '' }: Theme & { children: React.ReactNode; className?: string }) {
  const theme = stylex.props(...baselineTheme, scheme === 'light' && lightTheme)
  return <div data-workshop data-direction={direction} data-scheme={scheme} data-density={density} {...theme} className={`bg-canvas text-ink font-sans text-chrome ${className} ${theme.className}`}>{children}</div>
}

/* Icons — original inline strokes, no icon-font or asset dependencies. */
const icon = (path: React.ReactNode) => (props: React.SVGProps<SVGSVGElement>) => (
  <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" {...props}>{path}</svg>
)
export const CheckIcon = icon(<><path d="M3 8.5 6.5 12 13 4.5" /></>)
export const CopyIcon = icon(<><rect x="5.5" y="5.5" width="8" height="8" rx="1.5" /><path d="M10.5 3.5h-7a1 1 0 0 0-1 1v7" /></>)
export const XCircleIcon = icon(<><circle cx="8" cy="8" r="6" /><path d="M5.8 5.8l4.4 4.4M10.2 5.8l-4.4 4.4" /></>)

const toneText: Record<Tone, string> = { neutral: 'text-muted', good: 'text-good', warning: 'text-warning', danger: 'text-danger' }
const toneBg: Record<Tone, string> = { neutral: 'bg-recess', good: 'bg-good/12', warning: 'bg-warning/12', danger: 'bg-danger/12' }

/* Button — sizes, variants, icon-only square, disabled reasons. */
export type ButtonSize = 'sm' | 'md'
export function Button({ variant = 'secondary', size = 'md', square = false, className = '', ...props }: Aria.ButtonProps & { variant?: 'primary' | 'secondary' | 'quiet' | 'danger'; size?: ButtonSize; square?: boolean }) {
  const variantClass = variant === 'primary' ? 'bg-accent text-on-accent border-accent' : variant === 'quiet' ? 'bg-transparent border-transparent text-muted' : variant === 'danger' ? 'bg-transparent border-danger text-danger' : 'bg-panel border-line text-ink'
  const sizeClass = size === 'sm' ? 'min-h-[calc(var(--control-height)*0.78)] px-2 text-[11px]' : 'min-h-control px-3'
  return <Aria.Button {...props} className={`inline-flex items-center justify-center gap-2 rounded-control border font-semibold transition-colors cursor-pointer data-[hovered]:bg-selection data-[hovered]:text-ink data-[hovered]:border-line data-[pressed]:translate-y-px data-[disabled]:opacity-45 data-[disabled]:cursor-not-allowed ${square ? 'w-[var(--control-height)] px-0' : ''} ${sizeClass} ${variantClass} ${className}`} />
}

/* Pill / Badge / StatusDot / Spinner / Progress — labeled state surfaces. */
export function Pill({ children, tone = 'neutral' }: { children: React.ReactNode; tone?: Tone }) {
  return <span className={`inline-flex items-center gap-1.5 rounded-control px-2 py-0.5 text-[11px] font-semibold ${toneBg[tone]} ${toneText[tone]}`}>{children}</span>
}
export function Badge({ children, tone = 'neutral' }: { children: React.ReactNode; tone?: Tone }) {
  return <span className={`inline-flex items-center rounded-control border px-2 py-0.5 text-[11px] font-semibold ${tone === 'neutral' ? 'border-line text-muted' : tone === 'good' ? 'border-good text-good' : tone === 'warning' ? 'border-warning text-warning' : 'border-danger text-danger'}`}>{children}</span>
}
export type DotState = 'ready' | 'building' | 'queued' | 'error' | 'canceled'
// Plain spans: inside collection items, React Aria would adopt an unslotted <Text> as the item's label.
const dotStyle: Record<DotState, { text: string; dot: string }> = {
  ready: { text: 'text-good', dot: 'bg-good' },
  building: { text: 'text-accent', dot: 'bg-accent fui-pulse' },
  queued: { text: 'text-muted', dot: 'bg-muted' },
  error: { text: 'text-danger', dot: 'bg-danger' },
  canceled: { text: 'text-muted', dot: 'border border-muted' },
}
export function StatusDot({ state, label }: { state: DotState; label?: string }) {
  return <span className={`inline-flex items-center gap-2 ${dotStyle[state].text}`}><span aria-hidden="true" className={`size-1.5 rounded-full ${dotStyle[state].dot}`} />{label ?? state}</span>
}
export function Spinner({ label, size = 'md' }: { label: string; size?: ButtonSize }) {
  return <span role="status" className={`inline-flex items-center gap-2 text-muted ${size === 'sm' ? 'text-[11px]' : ''}`}><span aria-hidden="true" className="fui-spin inline-block size-3 rounded-full border-2 border-muted border-t-accent" />{label}</span>
}
export function Progress({ label, value = 0, maxValue = 100, tone = 'neutral', size = 'md' }: { label: string; value?: number; maxValue?: number; tone?: Tone; size?: ButtonSize }) {
  return <Aria.ProgressBar aria-label={label} value={value} maxValue={maxValue} className="w-full">
    {({ percentage, valueText }) => <div className="flex flex-col gap-1">
      <div className="flex justify-between text-[11px] text-muted"><span>{label}</span><span className="tabular">{valueText}</span></div>
      <div className={`w-full rounded-control bg-recess overflow-hidden ${size === 'sm' ? 'h-1' : 'h-1.5'}`}><div className={`h-full rounded-control transition-[width] duration-[var(--motion-duration)] ${tone === 'good' ? 'bg-good' : tone === 'danger' ? 'bg-danger' : tone === 'warning' ? 'bg-warning' : 'bg-accent'}`} style={{ width: `${percentage}%` }} /></div>
    </div>}
  </Aria.ProgressBar>
}

/* Kbd, Note, Description, Entity, EmptyState — static chrome families. */
export function Kbd({ children }: { children: React.ReactNode }) {
  return <kbd className="inline-flex min-w-5 items-center justify-center rounded-control border border-line bg-panel px-1.5 py-0.5 font-mono text-[11px] text-muted">{children}</kbd>
}
export function Note({ tone = 'neutral', filled = false, size = 'md', title, children }: { tone?: Tone; filled?: boolean; size?: ButtonSize; title?: string; children: React.ReactNode }) {
  return <div className={`rounded-control border px-3 py-2 text-[12px] leading-relaxed ${size === 'sm' ? 'text-[11px] py-1.5' : ''} ${filled ? `${toneBg[tone]} ${toneText[tone]} border-transparent` : `border-line ${toneText[tone]}`}`} role="note">{title ? <span className="mr-1.5 font-bold">{title}:</span> : null}{children}</div>
}
export function Description({ items }: { items: readonly (readonly [string, React.ReactNode])[] }) {
  return <dl className="grid grid-cols-[minmax(90px,auto)_minmax(0,1fr)] gap-x-3 gap-y-1.5 text-[12px]">{items.map(([term, value]) => <React.Fragment key={term}><dt className="text-muted whitespace-nowrap">{term}</dt><dd className="min-w-0">{value}</dd></React.Fragment>)}</dl>
}
export function Entity({ name, detail, tone = 'neutral' }: { name: string; detail?: React.ReactNode; tone?: Tone }) {
  const initials = name.split(/\s+/).slice(0, 2).map(word => word[0]?.toUpperCase() ?? '').join('')
  return <div className="flex min-w-0 items-center gap-2.5"><span className={`grid size-8 shrink-0 place-items-center rounded-control text-[12px] font-bold ${tone === 'neutral' ? 'bg-selection text-accent' : `${toneBg[tone]} ${toneText[tone]}`}`}>{initials}</span><span className="min-w-0"><span className="block truncate font-semibold">{name}</span>{detail ? <span className="block truncate text-[11px] text-muted">{detail}</span> : null}</span></div>
}
export function EmptyState({ title, hint, action }: { title: string; hint: string; action?: React.ReactNode }) {
  return <div className="grid place-items-center gap-2 rounded-panel border border-dashed border-line bg-recess px-6 py-8 text-center">
    <span aria-hidden="true" className="grid size-9 place-items-center rounded-control bg-panel border border-line text-muted">◌</span>
    <p className="font-semibold">{title}</p>
    <p className="max-w-72 text-[12px] text-muted leading-relaxed">{hint}</p>
    {action}
  </div>
}

/* Input / Toggle / Checkbox — form controls. */
export function Input({ label, value, onChange, placeholder, autoFocusKey = '/', clearKey = 'Escape' }: { label: string; value: string; onChange: (value: string) => void; placeholder?: string; autoFocusKey?: string | null; clearKey?: string | null }) {
  return <Aria.TextField aria-label={label} value={value} onChange={onChange} {...stylex.props(searchStyles.field)}>
    {({ isRequired }) => <><Aria.Input placeholder={placeholder} onKeyDown={event => { if (clearKey && value !== '' && event.key === clearKey) { event.preventDefault(); onChange('') } }} {...stylex.props(searchStyles.input)} />{value === '' && autoFocusKey ? <Kbd>{autoFocusKey}</Kbd> : <Button aria-label={`Clear ${label.toLowerCase()}`} square size="sm" variant="quiet" onPress={() => onChange('')}><XCircleIcon /></Button>}{isRequired ? null : null}</>}
  </Aria.TextField>
}
const searchStyles = stylex.create({
  field: { display: 'flex', width: '100%', minHeight: g.controlLg, alignItems: 'center', gap: s.md, borderRadius: r.control, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.controlBorder, backgroundColor: surface.raised, color: ink.fg, paddingInline: s.lg, ':focus-within': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  input: { width: '100%', minWidth: 0, borderWidth: 0, backgroundColor: surface.transparent, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, outline: 'none', '::placeholder': { color: ink.fgMuted, opacity: 1 } },
})
export function Toggle({ label, isSelected, onChange }: { label: string; isSelected: boolean; onChange: (selected: boolean) => void }) {
  return <Aria.Switch aria-label={label} isSelected={isSelected} onChange={onChange} className="inline-flex cursor-pointer items-center gap-2.5 text-[12px]">
    <span className={`relative inline-flex h-4.5 w-8 shrink-0 items-center rounded-full border transition-colors duration-[var(--motion-duration)] ${isSelected ? 'bg-accent border-accent' : 'bg-recess border-line'}`}><span aria-hidden="true" className={`absolute size-3 rounded-full bg-panel border transition-[left] duration-[var(--motion-duration)] ${isSelected ? 'left-[calc(100%-16px)] border-accent' : 'left-0.5 border-line'}`} /></span>
    {label}
  </Aria.Switch>
}
export function Checkbox({ label, isDisabled, isIndeterminate = false, defaultSelected }: { label: React.ReactNode; isDisabled?: boolean; isIndeterminate?: boolean; defaultSelected?: boolean }) {
  return <Aria.Checkbox isDisabled={isDisabled} isIndeterminate={isIndeterminate} defaultSelected={defaultSelected} className="group inline-flex cursor-pointer items-center gap-2.5 text-[12px] data-[disabled]:cursor-not-allowed data-[disabled]:opacity-45">
    <span className="grid size-4 place-items-center rounded-[calc(var(--radius)*0.6)] border border-line bg-panel text-on-accent transition-colors group-data-[selected]:border-accent group-data-[selected]:bg-accent group-data-[indeterminate]:border-accent group-data-[indeterminate]:bg-accent">
      <span className="hidden group-data-[selected]:block group-data-[indeterminate]:hidden"><CheckIcon width="11" height="11" strokeWidth="2.4" /></span>
      <span className="hidden group-data-[indeterminate]:block"><span aria-hidden="true" className="block h-0.5 w-2 rounded bg-on-accent" /></span>
    </span>
    {label}
  </Aria.Checkbox>
}

/* Tooltip, Menu, Modal, Tabs, Table — compound surfaces. */
export function Tooltip({ label, placement = 'bottom', delay = 350, children }: { label: string; placement?: 'top' | 'bottom'; delay?: number; children: React.ReactNode }) {
  return <Aria.TooltipTrigger delay={delay} closeDelay={0}>{children}<Aria.Tooltip placement={placement} offset={6} className="z-50 rounded-control bg-ink px-2.5 py-1.5 text-[11px] font-sans text-panel shadow-lg">{label}</Aria.Tooltip></Aria.TooltipTrigger>
}
export interface MenuItemSpec { id: string; label: string; kbd?: string; separatorBefore?: boolean }
export function Menu({ items, onAction, ariaLabel = 'Actions' }: { items: readonly MenuItemSpec[]; onAction?: (key: React.Key) => void; ariaLabel?: string }) {
  return <Aria.Menu aria-label={ariaLabel} onAction={onAction} className="min-w-52 p-1 outline-none">
    {items.map(item => <React.Fragment key={item.id}>
      {item.separatorBefore ? <Aria.Separator className="my-1 border-t border-line" /> : null}
      <Aria.MenuItem id={item.id} textValue={item.label} className="flex items-center justify-between gap-4 rounded-control px-3 py-unit cursor-pointer data-[focused]:bg-selection data-[focused]:outline-none"><span>{item.label}</span>{item.kbd ? <Kbd>{item.kbd}</Kbd> : null}</Aria.MenuItem>
    </React.Fragment>)}
  </Aria.Menu>
}
export function MenuTrigger({ items, onAction, ariaLabel, children }: { items: readonly MenuItemSpec[]; onAction?: (key: React.Key) => void; ariaLabel?: string; children: React.ReactNode }) {
  return <Aria.MenuTrigger>{children}<Aria.Popover placement="bottom end" className="border border-line rounded-panel bg-panel shadow-xl overflow-hidden"><Menu items={items} onAction={onAction} ariaLabel={ariaLabel} /></Aria.Popover></Aria.MenuTrigger>
}
export function DialogBody({ title, children, close }: { title: string; children: React.ReactNode; close: () => void }) {
  return <Aria.Dialog aria-label={title} className="outline-none p-5">
    <Aria.Heading slot="title" className="font-display text-xl font-semibold">{title}</Aria.Heading>
    <Aria.Button slot="close" aria-label="Close" className="absolute top-3 right-3 cursor-pointer text-muted data-[hovered]:text-ink"><XCircleIcon /></Aria.Button>
    {children}
  </Aria.Dialog>
}
export function Modal({ theme, title, children, trigger }: { theme: Theme; title: string; children: (close: () => void) => React.ReactNode; trigger?: React.ReactNode }) {
  return <Aria.DialogTrigger>{trigger ?? <Button variant="primary">{title}</Button>}
    <Aria.ModalOverlay isDismissable className="fixed inset-0 z-50 bg-black/50 flex items-center justify-center p-6 data-[entering]:animate-in">
      <Aria.Modal className="w-full max-w-md">{({ state }) => <ThemeRoot {...theme} className="relative rounded-panel border border-line bg-panel shadow-2xl"><DialogBody title={title} close={() => state.close()}>{children(() => state.close())}</DialogBody></ThemeRoot>}</Aria.Modal>
    </Aria.ModalOverlay>
  </Aria.DialogTrigger>
}
export function Tabs({ tabs, selectedKey, onSelectionChange, shouldForceMount = false, children }: { tabs: readonly { id: string; label: string }[]; selectedKey: string; onSelectionChange?: (key: React.Key) => void; shouldForceMount?: boolean; children: Record<string, React.ReactNode> }) {
  return <Aria.Tabs selectedKey={selectedKey} onSelectionChange={onSelectionChange}>
    <Aria.TabList aria-label="Views" className="flex gap-1 border-b border-line px-4">
      {tabs.map(tab => <Aria.Tab key={tab.id} id={tab.id} className="cursor-pointer border-b-2 border-transparent px-3 py-3 font-semibold text-muted capitalize data-[selected]:border-accent data-[selected]:text-ink data-[hovered]:text-ink transition-colors">{tab.label}</Aria.Tab>)}
    </Aria.TabList>
    {tabs.map(tab => <Aria.TabPanel key={tab.id} id={tab.id} shouldForceMount={shouldForceMount} className={selectedKey === tab.id ? '' : 'hidden'}>{children[tab.id]}</Aria.TabPanel>)}
  </Aria.Tabs>
}
export interface TableColumnSpec<Row> { id: string; label: string; render: (row: Row) => React.ReactNode; isRowHeader?: boolean }
export function Table<Row extends { id: string }>({ ariaLabel, columns, rows, compact = false, sticky = false }: { ariaLabel: string; columns: readonly TableColumnSpec<Row>[]; rows: readonly Row[]; compact?: boolean; sticky?: boolean }) {
  return <Aria.Table aria-label={ariaLabel} selectionMode="single" className={`w-full text-left border-collapse ${compact ? 'text-[12px]' : ''}`}>
    <Aria.TableHeader className={sticky ? 'sticky top-0 z-10 bg-panel' : ''}>
      {columns.map(column => <Aria.Column key={column.id} isRowHeader={column.isRowHeader} className="border-b border-line px-3 py-unit font-normal text-muted">{column.label}</Aria.Column>)}
    </Aria.TableHeader>
    <Aria.TableBody>{rows.map(row => <Aria.Row key={row.id} id={row.id} className="cursor-pointer data-[selected]:bg-selection">
      {columns.map(column => <Aria.Cell key={column.id} className="border-b border-line/60 px-3 py-unit">{column.render(row)}</Aria.Cell>)}
    </Aria.Row>)}</Aria.TableBody>
  </Aria.Table>
}

/* CodeBlock — filename, language, copy action. */
export function CodeBlock({ filename, language = 'text', code, size = 'md' }: { filename?: string; language?: string; code: string; size?: ButtonSize }) {
  const [copied, setCopied] = React.useState(false)
  return <div className="overflow-hidden rounded-panel border border-line bg-recess">
    <div className="flex items-center justify-between border-b border-line px-3 py-1.5">
      <span className="truncate font-mono text-[11px] text-muted">{filename ?? language}<span className="ml-2 rounded-control bg-panel px-1.5 py-0.5 uppercase">{language}</span></span>
      <Button aria-label={copied ? 'Copied' : `Copy ${filename ?? 'code'}`} square size="sm" variant="quiet" onPress={() => { setCopied(true); window.setTimeout(() => setCopied(false), 1200) }}>{copied ? <CheckIcon /> : <CopyIcon />}</Button>
    </div>
    <pre className={`overflow-x-auto p-3 font-mono leading-6 ${size === 'sm' ? 'text-[11px]' : 'text-[12px]'} text-ink`}>{code}</pre>
  </div>
}

/* CommandMenu — grouped, filterable, keyboard-first. */
export interface CommandItem { id: string; label: string; description?: string; keywords?: readonly string[]; kbd?: string; onSelect?: () => void }
export interface CommandGroup { id: string; label: string; items: readonly CommandItem[] }
export function CommandMenu({ groups, placeholder = 'Type a command…', emptyHint = 'No matching commands.', onAction }: { groups: readonly CommandGroup[]; placeholder?: string; emptyHint?: string; onAction?: (key: React.Key) => void }) {
  const [query, setQuery] = React.useState('')
  const [status, setStatus] = React.useState('')
  const normalized = query.trim().toLowerCase()
  const filtered = groups.map(group => ({ ...group, items: group.items.filter(item => !normalized || item.label.toLowerCase().includes(normalized) || item.description?.toLowerCase().includes(normalized) || item.keywords?.some(keyword => keyword.toLowerCase().includes(normalized))) })).filter(group => group.items.length > 0)
  // Autocomplete keeps DOM focus in the input while arrows/Enter drive the list (virtual focus).
  return <div className="overflow-hidden rounded-panel border border-line bg-panel">
    <Aria.Autocomplete inputValue={query} onInputChange={setQuery}>
      <Aria.SearchField aria-label="Find a command" {...stylex.props(searchStyles.field)}>
        <span aria-hidden="true" className="text-accent"><svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round"><circle cx="6.8" cy="6.8" r="4.6" /><path d="M10.4 10.4 14 14" /></svg></span>
        <Aria.Input placeholder={placeholder} {...stylex.props(searchStyles.input)} />
        {query !== '' ? <Button aria-label="Clear command search" square size="sm" variant="quiet" onPress={() => setQuery('')}><XCircleIcon /></Button> : <Kbd>/</Kbd>}
      </Aria.SearchField>
      <Aria.ListBox aria-label="Commands" onAction={key => { const item = groups.flatMap(group => group.items).find(candidate => candidate.id === key); item?.onSelect?.(); setStatus(item ? `Ran: ${item.label}` : ''); onAction?.(key) }} renderEmptyState={() => <p className="px-4 py-5 text-[12px] text-muted">{emptyHint}</p>} className="max-h-[min(28rem,60vh)] overflow-auto p-2">
        {filtered.map(group => <Aria.ListBoxSection key={group.id}>
          <Aria.Header className="px-3 pb-1 pt-2 text-[10px] font-semibold uppercase tracking-widest text-muted">{group.label}</Aria.Header>
          {group.items.map(item => <Aria.ListBoxItem key={item.id} id={item.id} textValue={item.label} className="flex cursor-pointer items-center justify-between gap-4 rounded-control px-3 py-unit data-[focused]:bg-selection data-[focused]:outline-none data-[hovered]:bg-recess">
            {/* Description stays reachable via aria-describedby; hiding it from the text walk keeps the name equal to the visible label (WCAG 2.5.3). */}
            <span className="min-w-0"><Aria.Text slot="label" className="block truncate font-semibold">{item.label}</Aria.Text>{item.description ? <Aria.Text slot="description" aria-hidden="true" className="block truncate text-[11px] text-muted">{item.description}</Aria.Text> : null}</span>
            {item.kbd ? <span aria-hidden="true"><Kbd>{item.kbd}</Kbd></span> : null}
          </Aria.ListBoxItem>)}
        </Aria.ListBoxSection>)}
      </Aria.ListBox>
    </Aria.Autocomplete>
    <p aria-live="polite" className="border-t border-line px-3 py-2 text-[11px] text-muted">{status || 'Arrow keys move · Enter runs · Escape clears'}</p>
  </div>
}

/* ContextCard — preview on hover AND keyboard focus (350 ms intent delay). */
export function ContextCard({ title, children, preview }: { title: string; children: React.ReactNode; preview: React.ReactNode }) {
  return <Aria.TooltipTrigger delay={350} closeDelay={0}>
    <Aria.Button className="w-full cursor-pointer rounded-panel border border-line bg-panel p-3 text-left transition-colors data-[hovered]:bg-recess data-[focus-visible]:outline data-[focus-visible]:outline-2 data-[focus-visible]:outline-offset-2 data-[focus-visible]:outline-accent">
      <span className="mb-1 block text-[10px] font-semibold uppercase tracking-widest text-accent">{title}</span>
      {children}
    </Aria.Button>
    <Aria.Tooltip placement="top start" offset={8} className="z-50 max-w-80 rounded-panel border border-line bg-panel p-3 text-[12px] font-sans leading-relaxed text-ink shadow-xl">{preview}</Aria.Tooltip>
  </Aria.TooltipTrigger>
}

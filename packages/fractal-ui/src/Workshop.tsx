import * as React from 'react'
import * as Aria from 'react-aria-components'
import { Badge, Button, Checkbox, CodeBlock, CommandMenu, ContextCard, Description, EmptyState, Entity, Input, Kbd, MenuTrigger, ModalDialog, Note, Pill, Progress, Spinner, StatusDot, Table, Tabs, Toggle, Tooltip, type CommandGroup, type Theme } from './kit'

export const directions = {
  folio: { name: 'Folio', index: '01', description: 'An annotated working notebook. Warm paper, plum ink, editorial headings.', character: 'Reflective / legible / deliberate', motion: '180 ms · soft ease', radius: '5 / 10 px', type: 'Trebuchet / Georgia / Courier' },
  relay: { name: 'Relay', index: '02', description: 'A precise dispatch desk. Cool mineral surfaces, teal signals, squared geometry.', character: 'Technical / compact / immediate', motion: '90 ms · linear', radius: '2 / 2 px', type: 'Arial / Arial / Lucida Console' },
  orbit: { name: 'Orbit', index: '03', description: 'A calm navigation instrument. Indigo layers, amber bearings, generous curves.', character: 'Composed / approachable / continuous', motion: '240 ms · settling ease', radius: '14 / 20 px', type: 'Verdana / Verdana / Consolas' },
} as const

function Choice({ label, value, options, onChange }: { label: string; value: string; options: readonly string[]; onChange: (value: string) => void }) {
  return <Aria.RadioGroup aria-label={label} value={value} onChange={onChange} className="flex flex-col gap-1.5">
    <Aria.Label className="text-[10px] font-semibold uppercase tracking-widest text-muted">{label}</Aria.Label>
    <div className="flex flex-wrap gap-1 rounded-control border border-line bg-panel p-1">
      {options.map(option => <Aria.Radio key={option} value={option} className="flex min-h-control cursor-pointer items-center rounded-control px-3 py-1 capitalize transition-colors data-[selected]:bg-accent data-[selected]:text-on-accent data-[hovered]:bg-selection data-[hovered]:text-ink">{option}</Aria.Radio>)}
    </div>
  </Aria.RadioGroup>
}

function Terminal() {
  return <section aria-label="Terminal pane" className="overflow-hidden rounded-panel border border-line bg-recess">
    <div className="flex items-center justify-between border-b border-line px-4 py-2">
      <span className="font-semibold">Terminal <span className="ml-2 font-normal text-muted">read-only fixture</span></span>
      <StatusDot state="ready" label="Exited 0" />
    </div>
    <pre className="overflow-x-auto p-4 font-mono text-[12px] leading-6">
      <span className="text-muted">workshop / cache-refresh{'\n'}</span>
      <span className="text-accent">$</span>{' pnpm vitest run src/cache.test.ts\n'}
      <span className="text-good">✓</span>{' retains cached values while refreshing\n'}
      <span className="text-good">✓</span>{' rejects expired refresh generations\n\n'}
      <span className="text-good">2 passed</span>{' · 184 ms · no network requests'}
    </pre>
  </section>
}

function Diff() {
  return <section aria-label="Unified diff" className="overflow-hidden rounded-panel border border-line bg-panel">
    <div className="flex justify-between border-b border-line px-4 py-2"><span className="font-mono text-[12px]">src/cache.ts</span><span className="text-muted">1 hunk</span></div>
    <pre className="overflow-x-auto font-mono text-[12px] leading-6">
      <span className="block bg-recess px-4 text-muted">@@ refresh(key) · lines 18–22 @@</span>
      <span className="block bg-removed px-4">−  return await load(key);</span>
      <span className="block bg-added px-4">+  const cached = store.get(key);</span>
      <span className="block bg-added px-4">+  void refreshInBackground(key);</span>
      <span className="block bg-added px-4">+  return cached ?? await load(key);</span>
    </pre>
  </section>
}

const threadMenuItems = [
  { id: 'pin', label: 'Pin thread', kbd: 'P' },
  { id: 'rename', label: 'Rename thread', kbd: 'R' },
  { id: 'copy', label: 'Copy thread link', kbd: '⇧C', separatorBefore: true },
  { id: 'archive', label: 'Archive thread', kbd: '⇧A' },
] as const

function ToolCall() {
  return <Aria.Disclosure defaultExpanded className="my-4 overflow-hidden rounded-control border border-line">
    <Button slot="trigger" className="w-full justify-between rounded-none border-0 bg-recess">
      <span>⌄ <span className="ml-2 font-mono text-[12px]">read_file</span> <span className="ml-2 font-normal text-muted">src/cache.ts</span></span>
      <StatusDot state="ready" label="Complete" />
    </Button>
    <Aria.DisclosurePanel className="px-4 py-3 font-mono text-[12px] text-muted">22 lines inspected · input: src/cache.ts · output: cached refresh flow</Aria.DisclosurePanel>
  </Aria.Disclosure>
}

function Conversation({ theme }: { theme: Theme }) {
  return <div className="flex flex-col gap-5 p-5">
    <article className="flex gap-3">
      <span className="grid size-8 shrink-0 place-items-center rounded-control bg-selection font-semibold text-accent">U</span>
      <div><p className="mb-1 text-[11px] text-muted">You <span className="ml-2">09:14</span></p><p className="text-[14px] leading-relaxed">Keep the cached result visible while the next refresh runs. Show the change before saving.</p></div>
    </article>
    <article className="flex gap-3">
      <span className="grid size-8 shrink-0 place-items-center rounded-control bg-accent font-semibold text-on-accent">A</span>
      <div className="min-w-0 flex-1">
        <p className="mb-1 text-[11px] text-muted">Assistant <span className="ml-2">09:15</span></p>
        <p className="text-[14px] leading-relaxed">The refresh now runs in the background. The visible result stays put until a newer value is ready.</p>
        <ToolCall />
        <Diff />
        <div className="mt-4 flex flex-wrap items-center gap-2">
          <ModalDialog theme={theme} title="Review checkpoint" trigger={<Button variant="primary">Review checkpoint</Button>}>{close => <>
            <p className="my-3 leading-relaxed text-muted">Save these two synthetic file changes for review. No repository will be written.</p>
            <div className="mb-4 flex justify-between rounded-control bg-recess p-3"><span>Cache refresh</span><Pill tone="good">2 files ready</Pill></div>
            <div className="flex justify-end gap-2"><Button onPress={close}>Keep editing</Button><Button variant="primary" onPress={close}>Save checkpoint</Button></div>
          </>}</ModalDialog>
          <Tooltip label="Inspect changes without saving" placement="top"><Button>Compare changes</Button></Tooltip>
          <Pill tone="good">Tests passed</Pill>
        </div>
      </div>
    </article>
    <div className="rounded-panel border border-line bg-recess p-3">
      <Aria.TextField aria-label="Message draft" className="block"><Aria.TextArea placeholder="Add a note or a next step…" className="min-h-12 w-full resize-y bg-transparent p-1 text-ink placeholder:text-muted" /></Aria.TextField>
      <div className="flex items-center justify-between text-[11px] text-muted"><span>Local workshop · no messages are sent</span><Button isDisabled>Send</Button></div>
    </div>
  </div>
}

const threads = [
  { id: 'cache', title: 'Keep cache results visible', detail: '2 files · ready to review', state: 'ready' as const, status: 'Review', time: '2m' },
  { id: 'search', title: 'Search keyboard navigation', detail: 'Waiting for a decision', state: 'error' as const, status: 'Attention', time: '8m' },
  { id: 'export', title: 'Export empty collections', detail: 'Checks are running', state: 'building' as const, status: 'Running', time: '12m' },
  { id: 'docs', title: 'Document retry boundaries', detail: 'Archived checkpoint', state: 'canceled' as const, status: 'Done', time: '1h' },
  { id: 'pins', title: 'Pin and reorder thread rows', detail: 'Queued behind review', state: 'queued' as const, status: 'Queued', time: '3h' },
] as const

function Sidebar() {
  return <aside className="border-b border-line bg-recess p-3 lg:border-b-0 lg:border-r">
    <div className="flex items-center justify-between px-2 py-3"><span className="text-[10px] font-semibold uppercase tracking-widest text-muted">Example project</span><Pill>5 threads</Pill></div>
    <Aria.ListBox aria-label="Threads" selectionMode="single" defaultSelectedKeys={['cache']} className="flex flex-col gap-2">
      {threads.map(thread => <Aria.ListBoxItem key={thread.id} id={thread.id} textValue={thread.title} className="cursor-pointer rounded-control border border-transparent p-unit data-[selected]:border-line data-[selected]:bg-panel data-[hovered]:bg-selection">
        <div className="mb-1 flex justify-between gap-2 font-semibold"><span className="truncate">{thread.title}</span><span className="text-[10px] font-normal text-muted tabular">{thread.time}</span></div>
        <p className="mb-2 text-[11px] text-muted">{thread.detail}</p>
        <StatusDot state={thread.state} label={thread.status} />
      </Aria.ListBoxItem>)}
    </Aria.ListBox>
    <div className="mt-4 space-y-3 border-t border-line px-2 pt-3">
      <ContextCard title="Attention" preview={<>Three threads need a human decision. The oldest has waited 8 minutes; nothing is lost by waiting longer.</>} >
        <div className="flex items-center justify-between"><StatusDot state="error" label="3 decisions" /><span className="text-[11px] text-muted">hover or focus</span></div>
      </ContextCard>
      <p className="text-[11px] leading-relaxed text-muted">Activity is separate from usage.<br />Quota: unavailable in this fixture.</p>
    </div>
  </aside>
}

const reviewColumns = [
  { id: 'file', label: 'File', isRowHeader: true, render: (row: { id: string; file: string; change: React.ReactNode; review: React.ReactNode }) => <span className="font-mono">{row.file}</span> },
  { id: 'change', label: 'Change', render: (row: { id: string; file: string; change: React.ReactNode; review: React.ReactNode }) => <span className="font-mono tabular">{row.change}</span> },
  { id: 'review', label: 'Review', render: (row: { id: string; file: string; change: React.ReactNode; review: React.ReactNode }) => row.review },
] as const
const reviewRows = [
  { id: 'cache', file: 'src/cache.ts', change: <span><span className="text-good">+4</span> <span className="text-danger">−1</span></span>, review: <StatusDot state="ready" label="Ready" /> },
  { id: 'test', file: 'src/cache.test.ts', change: <span className="text-good">+12</span>, review: <StatusDot state="ready" label="Ready" /> },
  { id: 'index', file: 'src/index.ts', change: <span><span className="text-good">+2</span> <span className="text-danger">−2</span></span>, review: <StatusDot state="building" label="Checking" /> },
] as const

const commandGroups: readonly CommandGroup[] = [
  { id: 'navigate', label: 'Navigate', items: [
    { id: 'open-review', label: 'Open changed files', description: 'Review · 3 files', keywords: ['diff', 'review'], kbd: 'R' },
    { id: 'open-terminal', label: 'Show terminal output', description: 'Execution · read only', keywords: ['shell', 'logs'], kbd: 'T' },
    { id: 'pin', label: 'Pin this thread', description: 'Navigation · current thread', keywords: ['sidebar'], kbd: 'P' },
  ] },
  { id: 'work', label: 'Work', items: [
    { id: 'checkpoint', label: 'Save review checkpoint', description: 'Keeps current changes for review', keywords: ['save', 'snapshot'], kbd: '⇧S' },
    { id: 'draft', label: 'Open message draft', description: 'Continue an unsent note', keywords: ['composer'], kbd: '⇧D' },
  ] },
]

export function Workshop({ direction = 'folio', scheme = 'light', density = 'comfortable', onThemeChange }: Theme & { onThemeChange?: (change: Partial<Theme>) => void }) {
  const theme = { direction, scheme, density }
  const info = directions[direction]
  const [tab, setTab] = React.useState<React.Key>('conversation')
  const [pinned, setPinned] = React.useState(true)
  const [filter, setFilter] = React.useState('')
  return <div data-workshop data-direction={direction} data-scheme={scheme} data-density={density} className="min-h-screen bg-canvas p-4 font-sans text-chrome text-ink md:p-7">
    <header className="mx-auto flex max-w-[1500px] flex-wrap items-center justify-between gap-6 border-b border-line pb-5">
      <div><p className="mb-2 text-[10px] uppercase tracking-[.2em] text-muted">Fractal UI / visual language workshop</p><h1 className="font-display text-3xl font-semibold">{info.index} <span className="text-accent">{info.name}</span></h1></div>
      <div className="flex flex-wrap gap-4">
        <Choice label="Direction" value={direction} options={['folio', 'relay', 'orbit']} onChange={value => onThemeChange?.({ direction: value as Theme['direction'] })} />
        <Choice label="Scheme" value={scheme} options={['light', 'dark']} onChange={value => onThemeChange?.({ scheme: value as Theme['scheme'] })} />
        <Choice label="Density" value={density} options={['compact', 'comfortable']} onChange={value => onThemeChange?.({ density: value as Theme['density'] })} />
      </div>
    </header>
    <main className="mx-auto max-w-[1500px]">
      <div className="flex flex-wrap items-center justify-between gap-2 py-5"><p className="text-muted">{info.description}</p><span className="text-[10px] font-semibold uppercase tracking-widest text-accent">{info.character}</span></div>
      <div className="grid items-start gap-5 xl:grid-cols-[minmax(0,1fr)_340px]">
        <section aria-label="Working context" className="overflow-hidden rounded-panel border border-line bg-panel">
          <div className="flex items-center justify-between border-b border-line px-5 py-3">
            <div><h2 className="font-display text-lg font-semibold">Cache refresh</h2><p className="text-[11px] text-muted">example-project / isolated worktree / synthetic session</p></div>
            <MenuTrigger items={threadMenuItems} ariaLabel="Thread actions"><Button aria-label="Thread actions" square>•••</Button></MenuTrigger>
          </div>
          <div className="grid lg:grid-cols-[250px_minmax(0,1fr)]">
            <Sidebar />
            <div className="min-w-0">
              <Tabs tabs={[{ id: 'conversation', label: 'conversation' }, { id: 'review', label: 'review' }, { id: 'terminal', label: 'terminal' }]} selectedKey={String(tab)} onSelectionChange={key => setTab(String(key))} shouldForceMount children={{
                conversation: <Conversation theme={theme} />,
                review: <div className="p-4"><Table ariaLabel="Changed files" columns={reviewColumns} rows={reviewRows} compact /><div className="mt-4"><Diff /></div></div>,
                terminal: <div className="p-4"><Terminal /></div>,
              }} />
              <div className="border-t border-line px-5 py-4"><div className="mb-2 flex items-center justify-between"><span className="text-[11px] font-semibold uppercase tracking-widest text-muted">Thread settings</span><Toggle label="Pinned" isSelected={pinned} onChange={setPinned} /></div><Input label="Filter thread output" value={filter} onChange={setFilter} placeholder="Filter by file or status" /></div>
            </div>
          </div>
        </section>
        <aside className="flex flex-col gap-5">
          <section><h2 className="mb-3 font-display text-lg font-semibold">Command palette</h2><CommandMenu groups={commandGroups} /></section>
          <section className="rounded-panel border border-line bg-panel p-4">
            <h2 className="mb-3 font-display text-lg font-semibold">Direction recipe</h2>
            <Description items={[['Type', info.type], ['Density', `${density} · ${direction === 'relay' ? '−1' : direction === 'folio' ? '+1' : '+2'} px bias`], ['Radius', info.radius], ['Motion', info.motion], ['Status', <Badge tone="warning">Selection pending</Badge>]]} />
            <div className="mt-4 flex gap-2">{(['canvas', 'recess', 'ink', 'accent', 'good', 'danger'] as const).map(color => <div key={color} title={color} className={`size-7 rounded-control border border-line ${color === 'canvas' ? 'bg-canvas' : color === 'recess' ? 'bg-recess' : color === 'ink' ? 'bg-ink' : color === 'accent' ? 'bg-accent' : color === 'good' ? 'bg-good' : 'bg-danger'}`} />)}</div>
            <div className="mt-4"><Note tone="neutral" size="sm">Same content and controls in every direction. Motion stops under reduced-motion preference.</Note></div>
            <div className="mt-3"><Progress label="Review readiness" value={66} tone="warning" size="sm" /></div>
          </section>
        </aside>
      </div>
      <section aria-label="Component specimens" className="mt-6">
        <div className="mb-3 flex items-center justify-between"><h2 className="font-display text-xl font-semibold">The working parts</h2><p className="text-[11px] text-muted">React Aria primitives · keyboard-first · all 22 families in the Families story</p></div>
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
          <section className="rounded-panel border border-line bg-panel p-4">
            <h3 className="mb-3 text-[10px] font-semibold uppercase tracking-widest text-muted">Button / Pill / Badge / StatusDot / Spinner</h3>
            <div className="mb-3 flex flex-wrap gap-2"><Button variant="primary">Continue</Button><Button>Secondary</Button><Button variant="danger">Discard</Button><Button isDisabled>Unavailable</Button></div>
            <div className="mb-3 flex flex-wrap gap-2"><Pill tone="good">Complete</Pill><Pill tone="warning">Needs review</Pill><Pill tone="danger">Failed</Pill><Badge>neutral</Badge><Badge tone="good">passing</Badge></div>
            <div className="mb-3 flex flex-wrap gap-3"><StatusDot state="ready" /><StatusDot state="building" /><StatusDot state="queued" /><StatusDot state="error" /><StatusDot state="canceled" /></div>
            <Spinner label="Loading preview…" size="sm" />
            <div className="mt-4"><Tooltip label="A checkpoint keeps your current changes"><Button>Hover or focus for tooltip</Button></Tooltip></div>
          </section>
          <section className="rounded-panel border border-line bg-panel p-4">
            <h3 className="mb-3 text-[10px] font-semibold uppercase tracking-widest text-muted">Menu / Kbd / Toggle / Checkbox</h3>
            <div className="mb-3 flex flex-wrap items-center gap-2"><MenuTrigger items={threadMenuItems}><Button>Thread actions</Button></MenuTrigger><Kbd>⌘K</Kbd><Kbd>G</Kbd><Kbd>B</Kbd></div>
            <div className="flex flex-col items-start gap-2.5"><Toggle label="Compact rows" isSelected={density === 'compact'} onChange={() => onThemeChange?.({ density: density === 'compact' ? 'comfortable' : 'compact' })} /><Checkbox label="Notify on attention" defaultSelected /><Checkbox label="Permission denied by owner" isDisabled /><Checkbox label="Mixed selection" isIndeterminate /></div>
          </section>
          <section className="overflow-hidden rounded-panel border border-line bg-panel">
            <h3 className="px-4 pb-3 pt-4 text-[10px] font-semibold uppercase tracking-widest text-muted">Table / Entity / Description</h3>
            <Table ariaLabel="Agents" compact columns={[{ id: 'agent', label: 'Agent', isRowHeader: true, render: (row: { id: string; name: string; detail: string }) => <Entity name={row.name} detail={row.detail} /> }, { id: 'state', label: 'State', render: (row: { id: string; name: string; detail: string; state: React.ReactNode }) => row.state }]} rows={[{ id: 'atlas', name: 'Atlas', detail: 'review lane · 2 files', state: <StatusDot state="building" label="Working" /> }, { id: 'birch', name: 'Birch', detail: 'idle · waiting for input', state: <StatusDot state="queued" label="Idle" /> }]} />
            <div className="p-4"><Description items={[['Worktree', <span className="font-mono">isolated/synthetic</span>], ['Session', 'fixture · not live'], ['Cost', <span className="tabular">unavailable</span>]]} /></div>
          </section>
          <section className="rounded-panel border border-line bg-panel p-4">
            <h3 className="mb-3 text-[10px] font-semibold uppercase tracking-widest text-muted">CodeBlock / Note / EmptyState / ContextCard</h3>
            <div className="mb-3"><CodeBlock filename="fixture.json" language="json" size="sm" code={'{\n  "synthetic": true,\n  "captures": 0\n}'} /></div>
            <div className="mb-3"><Note tone="warning" filled size="sm" title="Heads up">Fixtures are synthetic; no live data is loaded.</Note></div>
            <EmptyState title="No captures yet" hint="This workshop renders synthetic content only." action={<Button size="sm">Learn why</Button>} />
          </section>
        </div>
      </section>
      <footer className="mt-5 flex flex-wrap justify-between gap-2 text-[10px] text-muted"><span>Original visual study · synthetic data only · no product affiliation</span><span>Folio / Relay / Orbit — selection pending</span></footer>
    </main>
  </div>
}

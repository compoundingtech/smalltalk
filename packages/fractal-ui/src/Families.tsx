import * as React from 'react'
import { Badge, Button, Checkbox, CheckIcon, CodeBlock, CommandMenu, ContextCard, CopyIcon, Description, EmptyState, Entity, Input, Kbd, MenuTrigger, ModalDialog, Note, Pill, Progress, Spinner, StatusDot, Table, Tabs, Toggle, Tooltip, XCircleIcon, type CommandGroup, type Theme } from './kit'

const familyLabel = 'text-[10px] font-semibold uppercase tracking-widest text-muted mb-3'
function Family({ name, note, children }: { name: string; note?: string; children: React.ReactNode }) {
  return <section className="rounded-panel border border-line bg-panel p-4"><h2 className={familyLabel}>{name}</h2>{children}{note ? <p className="mt-3 text-[11px] leading-relaxed text-muted">{note}</p> : null}</section>
}

const menuItems = [{ id: 'pin', label: 'Pin thread', kbd: 'P' }, { id: 'copy', label: 'Copy link', kbd: '⇧C' }, { id: 'archive', label: 'Archive', kbd: '⇧A', separatorBefore: true }] as const
const commandGroups: readonly CommandGroup[] = [
  { id: 'go', label: 'Go to', items: [{ id: 'review', label: 'Open review', description: '3 changed files', kbd: 'R' }, { id: 'terminal', label: 'Open terminal', description: 'Read-only output', kbd: 'T' }] },
  { id: 'do', label: 'Do', items: [{ id: 'checkpoint', label: 'Save checkpoint', description: 'Keeps current changes', kbd: '⇧S' }, { id: 'draft', label: 'Open draft', description: 'Unsent note', kbd: '⇧D' }, { id: 'search', label: 'Search everything', description: 'Threads, files, commands', keywords: ['find'], kbd: '/' }] },
]
const tableColumns = [
  { id: 'file', label: 'File', isRowHeader: true, render: (row: { id: string; file: string; change: React.ReactNode; state: React.ReactNode }) => <span title={row.file} className="block max-w-48 truncate font-mono">{row.file}</span> },
  { id: 'change', label: 'Change', render: (row: { id: string; file: string; change: React.ReactNode; state: React.ReactNode }) => <span className="font-mono tabular">{row.change}</span> },
  { id: 'state', label: 'State', render: (row: { id: string; file: string; change: React.ReactNode; state: React.ReactNode }) => row.state },
] as const
const tableRows = [
  { id: 'one', file: 'src/one.ts', change: <span><span className="text-good">+4</span> <span className="text-danger">−1</span></span>, state: <StatusDot state="ready" label="Ready" /> },
  { id: 'two', file: 'src/long-filename-that-truncates-in-narrow-panes.test.ts', change: <span className="text-good">+12</span>, state: <StatusDot state="building" label="Checking" /> },
  { id: 'three', file: 'src/three.ts', change: <span className="text-danger">−8</span>, state: <StatusDot state="error" label="Failed" /> },
] as const

/** Every inventory family on one canvas, under the active direction and density. */
export function Families({ direction = 'folio', scheme = 'light', density = 'comfortable', onThemeChange }: Theme & { onThemeChange?: (change: Partial<Theme>) => void }) {
  const theme = { direction, scheme, density }
  const [tab, setTab] = React.useState<React.Key>('editor')
  const [input, setInput] = React.useState('')
  const [toggle, setToggle] = React.useState(true)
  return <div data-workshop data-direction={direction} data-scheme={scheme} data-density={density} className="min-h-screen bg-canvas p-4 font-sans text-chrome text-ink md:p-7">
    <header className="mx-auto max-w-[1400px] border-b border-line pb-5">
      <p className="mb-2 text-[10px] uppercase tracking-[.2em] text-muted">Fractal UI / component families</p>
      <h1 className="font-display text-3xl font-semibold">Twenty-two families, one language</h1>
      <div className="mt-4 flex flex-wrap gap-3">
        {(['folio', 'relay', 'orbit'] as const).map(option => <Button key={option} onPress={() => onThemeChange?.({ direction: option })} variant={direction === option ? 'primary' : 'secondary'}>{option}</Button>)}
        <Button onPress={() => onThemeChange?.({ scheme: scheme === 'dark' ? 'light' : 'dark' })} variant="quiet">Switch to {scheme === 'dark' ? 'light' : 'dark'}</Button>
        <Button onPress={() => onThemeChange?.({ density: density === 'compact' ? 'comfortable' : 'compact' })} variant="quiet">Switch to {density === 'compact' ? 'comfortable' : 'compact'}</Button>
      </div>
    </header>
    <main className="mx-auto grid max-w-[1400px] gap-4 py-6 md:grid-cols-2 xl:grid-cols-3">
      <Family name="Button" note="sm and md heights come from density; square icon-only keeps hit target; disabled states explain themselves.">
        <div className="flex flex-wrap gap-2"><Button variant="primary">Primary</Button><Button>Secondary</Button><Button variant="quiet">Quiet</Button><Button variant="danger">Danger</Button></div>
        <div className="mt-3 flex flex-wrap gap-2"><Button size="sm">Small</Button><Button size="sm" variant="primary">Small primary</Button><Button isDisabled>Disabled</Button><Tooltip label="Copy the fixture link"><Button square aria-label="Copy link"><CopyIcon /></Button></Tooltip></div>
      </Family>
      <Family name="Badge / Pill" note="Labeled semantic status tags; tone is never the only signal.">
        <div className="flex flex-wrap gap-2"><Badge>draft</Badge><Badge tone="good">passing</Badge><Badge tone="warning">at risk</Badge><Badge tone="danger">failing</Badge></div>
        <div className="mt-3 flex flex-wrap gap-2"><Pill>queued</Pill><Pill tone="good">ready</Pill><Pill tone="warning">needs decision</Pill><Pill tone="danger">failed</Pill></div>
      </Family>
      <Family name="StatusDot / Spinner / Progress">
        <div className="flex flex-wrap gap-x-5 gap-y-2"><StatusDot state="ready" /><StatusDot state="building" /><StatusDot state="queued" /><StatusDot state="error" /><StatusDot state="canceled" /></div>
        <div className="mt-3"><Spinner label="Fetching preview" /></div>
        <div className="mt-3 space-y-3"><Progress label="Checks" value={4} maxValue={6} tone="warning" /><Progress label="Upload" value={100} tone="good" size="sm" /></div>
      </Family>
      <Family name="CodeBlock" note="Plain text and JSON with filename plus copy; language chip; horizontal scroll for long lines.">
        <CodeBlock filename="session.fixture.json" language="json" size="sm" code={'{\n  "kind": "synthetic",\n  "captured": false,\n  "rows": [{ "id": "one", "state": "ready" }]\n}'} />
      </Family>
      <Family name="CommandMenu" note="Grouped items with description and keywords; keyboard-first; honest empty state.">
        <CommandMenu groups={commandGroups} placeholder="Search everything…" />
      </Family>
      <Family name="ContextCard" note="Preview appears on hover or keyboard focus after a 350 ms intent delay.">
        <ContextCard title="Why synthetic" preview={<>Fixtures are authored, not captured. No live session, host or credential can leak into a story.</>}>
          <div className="flex items-center justify-between"><span className="font-semibold">Fixture policy</span><StatusDot state="ready" label="Clean" /></div>
        </ContextCard>
      </Family>
      <Family name="Description / Entity" note="Metadata pairs and named actors with stable identity chips.">
        <Description items={[['Worktree', <span className="font-mono">isolated/example</span>], ['Started', <span className="tabular">09:14</span>], ['Model', 'fixture · unlabeled'], ['Cost', <span className="text-muted">unavailable</span>]]} />
        <div className="mt-4 flex flex-col items-start gap-2.5"><Entity name="Atlas" detail="review lane · 2 files" /><Entity name="Birch" detail="idle · waiting for input" tone="warning" /></div>
      </Family>
      <Family name="Input / Kbd / Toggle / Checkbox" note="Controlled input with slash-focus hint and Escape clear; opt-in checkboxes with honest disabled reasons.">
        <Input label="Filter" value={input} onChange={setInput} placeholder="Filter files…" />
        <div className="mt-3 flex flex-wrap items-center gap-2"><Kbd>⌘</Kbd><Kbd>K</Kbd><span className="text-[11px] text-muted">opens the palette from anywhere</span></div>
        <div className="mt-3 flex flex-col items-start gap-2.5"><Toggle label="Watch thread" isSelected={toggle} onChange={setToggle} /><Checkbox label="Include archived" defaultSelected /><Checkbox label="Permission denied by owner" isDisabled /><Checkbox label="Partial selection" isIndeterminate /></div>
      </Family>
      <Family name="Menu" note="Actions with shortcut hints; separators group destructive entries.">
        <div className="flex flex-wrap gap-2"><MenuTrigger items={menuItems}><Button>Thread actions</Button></MenuTrigger><MenuTrigger items={menuItems}><Button size="sm">Compact menu</Button></MenuTrigger></div>
      </Family>
      <Family name="Modal" note="Focus trap, Escape and scrim dismissal; primary and secondary actions.">
        <ModalDialog theme={theme} title="Save checkpoint?" trigger={<Button variant="primary">Open dialog</Button>}>{close => <>
          <p className="my-3 leading-relaxed text-muted">Three synthetic files stay staged. Nothing outside this story changes.</p>
          <div className="flex justify-end gap-2"><Button onPress={close}>Keep editing</Button><Button variant="primary" onPress={close}>Save checkpoint</Button></div>
        </>}</ModalDialog>
      </Family>
      <Family name="Note / EmptyState" note="Severity notes and an honest empty state with a way forward.">
        <div className="space-y-2"><Note tone="neutral">Neutral context note.</Note><Note tone="warning" filled title="Attention">One thread waits on your decision.</Note><Note tone="danger" size="sm">The last check failed; retry is safe.</Note></div>
        <div className="mt-3"><EmptyState title="No threads match" hint="Filters hide everything here. Clearing them restores the five synthetic threads." action={<Button size="sm" onPress={() => setInput('')}>Clear filter</Button>} /></div>
      </Family>
      <Family name="Table" note="Compact rows, single replace-selection, row-header semantics, truncation in narrow panes.">
        <Table ariaLabel="Changed files" columns={tableColumns} rows={tableRows} compact />
      </Family>
      <Family name="Tabs" note="Controlled selection; hidden panels keep their state via force-mount.">
        <div className="overflow-hidden rounded-panel border border-line">
          <Tabs tabs={[{ id: 'editor', label: 'editor' }, { id: 'output', label: 'output' }, { id: 'settings', label: 'settings' }]} selectedKey={String(tab)} onSelectionChange={key => setTab(String(key))} shouldForceMount children={{
            editor: <div className="p-4"><CodeBlock filename="scratch.ts" language="text" size="sm" code={'// typed while the output tab was never opened\nexport const keep = "me mounted"' } /></div>,
            output: <div className="p-4"><TerminalStub /></div>,
            settings: <div className="flex flex-col items-start gap-2.5 p-4"><Toggle label="Word wrap" isSelected={toggle} onChange={setToggle} /><Checkbox label="Run checks on save" defaultSelected /></div>,
          }} />
        </div>
      </Family>
      <Family name="Tooltip" note="Terse affordance labels; focus and hover share the same delay.">
        <div className="flex flex-wrap gap-3"><Tooltip label="Pin the active thread"><Button size="sm">Top</Button></Tooltip><Tooltip label="Archive after review" placement="bottom"><Button size="sm">Bottom</Button></Tooltip></div>
      </Family>
      <Family name="Icons" note="Original inline strokes for check, copy and error; currentColor, no assets.">
        <div className="flex flex-wrap items-center gap-4 text-[12px]"><span className="flex items-center gap-2"><CheckIcon /> copied</span><span className="flex items-center gap-2"><CopyIcon /> copy</span><span className="flex items-center gap-2 text-danger"><XCircleIcon /> failed</span></div>
      </Family>
    </main>
    <footer className="mx-auto max-w-[1400px] border-t border-line pt-4 text-[10px] text-muted">All families render from the same tokens in all three directions, both schemes and both densities.</footer>
  </div>
}

function TerminalStub() {
  return <pre className="overflow-x-auto rounded-panel border border-line bg-recess p-3 font-mono text-[12px] leading-6"><span className="text-accent">$</span>{' pnpm vitest run\n'}<span className="text-good">✓</span>{' 2 passed · 184 ms'}</pre>
}

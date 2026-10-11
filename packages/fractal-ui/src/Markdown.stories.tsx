import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { flushSync } from 'react-dom'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { Button } from 'react-aria-components'
import { Markdown, ResourceChip, completeStreamingTail, type InlineResource } from './assistant-ui/composition/Markdown'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme } from './assistant-ui/composition-theme'
import { ThemePortal } from './assistant-ui/taste/ThemePortal'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'

const richText = [
  '# [**Selection** and *ordering*](https://example.com/docs/selection_(stable) "Selection guide")',
  '',
  '**Bold with *nested italic***, __alternate bold__, *italic*, _alternate italic_, and escaped \\*literal stars\\*. Inline code keeps `a ** b` literal.',
  '',
  '3. Preserve the selected row.',
  '   - Keep **identity** stable.',
  '     1. Read `src/model.ts`.',
  '     2. Inspect src/selection.ts.',
  '   - Restore focus after sorting.',
  '4. Confirm the rendered order.',
  '',
  '| Resource | State | Count |',
  '| :--- | :---: | ---: |',
  '| `src/model.ts` | **Ready** | 3 |',
  '| [Selection *guide*](https://example.com/guide "Detailed guide") | *Streaming* | 12 |',
  '| Escaped \\| pipe | `a ** b` | 0 |',
].join('\n')
const languages = [
  ['ts', 'export const selected: string = "row-3"'],
  ['tsx', 'export const Row = ({ title }: { title: string }) => <strong>{title}</strong>'],
  ['js', '// Preserve identity\nconst rows = items.filter(item => item.ready)'],
  ['json', '{ "ready": true, "count": 3 }'],
  ['shell', 'printf "%s\\n" "$HOME"\nfor file in src/*.ts; do echo "$file"; done'],
  ['diff', '--- a/src/model.ts\n+++ b/src/model.ts\n@@ -1 +1 @@\n-old\n+new'],
  ['rust', 'fn main() { let selected: Option<usize> = Some(3); println!("{:?}", selected); }'],
  ['nix', '{ pkgs, ... }: { packages = [ pkgs.nodejs ]; enabled = true; }'],
  ['python', 'def selected(rows):\n    return [row for row in rows if row["ready"]]'],
  ['yaml', 'selected: row-3\nready: true\nresources:\n  - src/model.ts'],
  ['markdown', '# Selection\n**Stable** identity and [a guide](https://example.com).'],
  ['css', '.selected { color: var(--foreground); display: grid; }'],
  ['unknown-language', '<script>alert("This stays literal code")</script>'],
  ['', 'A fence without a language stays plain text.'],
] as const
const languageText = languages.map(([language, code]) => `\`\`\`${language}\n${code}\n\`\`\``).join('\n\n')
const streamChunks = [
  '# Streaming result\n\n```typescript\nconst selected = "row-3";',
  '\nconst description = "A long row title that wraps only after the reader enables wrapping and stays wrapped as new tokens arrive in the same code fence.";',
  '\nexport const state = { selected, description };',
  '\n```\n\n**Complete.**\n\n```typescript\nexport const complete = true;\n```',
] as const
const inlineStreamStages = [
  '**Stable',
  '**Stable** and *ordered',
  '**Stable** and *ordered* in `src/model.ts',
  '**Stable** and *ordered* in `src/model.ts` — [**Selection** guide](https://example.com/gui',
  '**Stable** and *ordered* in `src/model.ts` — [**Selection** guide](https://example.com/guide "Selection guide")',
] as const
const resources: readonly InlineResource[] = [{ path: 'src/model.ts', added: 3, removed: 1, preview: 'export const selected = "row-3"' }]
const customReference: InlineResource = { path: 'src/selection.ts', added: 2, removed: 0 }
const renderInlineReference = (path: string) => path === customReference.path ? <span data-testid="custom-inline-reference"><ResourceChip resource={customReference} /></span> : undefined

type Scenario = 'rich-text' | 'languages' | 'streaming' | 'inline-streaming' | 'all'
function MarkdownCanvas({ scenario = 'rich-text', scheme = 'dark' }: { scenario?: Scenario; scheme?: 'dark' | 'light' }) {
  const [stage, setStage] = React.useState(0)
  const [opened, setOpened] = React.useState<string | undefined>()
  const openResource = React.useCallback((path?: string) => setOpened(path), [])
  const streaming = scenario === 'streaming' || scenario === 'inline-streaming'
  const stages = scenario === 'inline-streaming' ? inlineStreamStages : streamChunks
  const text = scenario === 'languages' ? languageText : scenario === 'streaming' ? streamChunks.slice(0, stage + 1).join('') : scenario === 'inline-streaming' ? inlineStreamStages[stage]! : richText
  return <main data-testid="markdown-story" data-stream-stage={stage} {...stylex.props(styles.canvas, ...baselineTheme, scheme === 'light' && lightTheme)}><ThemePortal>
    <div {...stylex.props(styles.lane)}>
      {streaming ? <div {...stylex.props(styles.toolbar)}><Button data-testid="append-stream" onPress={() => setStage(value => value + 1)} isDisabled={stage === stages.length - 1} {...stylex.props(styles.button)}>Append stream chunk</Button><span>{stage === stages.length - 1 ? 'Complete' : 'Streaming'}</span></div> : null}
      <Markdown text={text} streaming={streaming && stage < stages.length - 1} resources={resources} onOpenResource={openResource} renderInlineReference={renderInlineReference} />
      {opened ? <output data-testid="opened-resource">Opened {opened}</output> : null}
      {scenario === 'all' ? <><h2 {...stylex.props(styles.heading)}>Code languages</h2><Markdown text={languageText} /><h2 {...stylex.props(styles.heading)}>Open streaming fence</h2><Markdown text={streamChunks[0]} streaming /><h2 {...stylex.props(styles.heading)}>Partial inline streams</h2>{inlineStreamStages.map(value => <Markdown key={value} text={value} streaming resources={resources} />)}</> : null}
    </div>
  </ThemePortal></main>
}
const meta = {
  title: 'Fractal UI/Markdown', component: MarkdownCanvas,
  parameters: { layout: 'fullscreen', docs: { description: { component: 'CommonMark + GFM prose with semantic emphasis, nested lists, aligned scrollable tables, titled links, and preserved resource references. Code fences retain local wrap/copy state across streamed updates, copy unformatted source through the Clipboard API, and lazily load explicit refractor grammars whose tokens use the existing semantic theme. Unknown languages remain plain text. Streaming mode completes unfinished inline prose without changing settled literal punctuation or fenced source.' } } },
  args: { scenario: 'rich-text', scheme: 'dark' },
  argTypes: { scenario: { options: ['rich-text', 'languages', 'streaming', 'inline-streaming', 'all'], control: 'radio' }, scheme: { options: ['dark', 'light'], control: 'radio' } },
} satisfies Meta<typeof MarkdownCanvas>
export default meta
type Story = StoryObj<typeof meta>

const settle = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
const requireElement = <T extends Element>(root: Element, selector: string): T => {
  const element = root.querySelector<T>(selector)
  if (element === null) throw new Error(`Missing ${selector}`)
  return element
}

function streamingTailStory(text: string, terminalSelector: string): Story {
  return {
    render: ({ scheme }) => <main data-testid="markdown-story" {...stylex.props(styles.canvas, ...baselineTheme, scheme === 'light' && lightTheme)}><ThemePortal><div {...stylex.props(styles.lane)}><Markdown text={text} streaming /></div></ThemePortal></main>,
    play: async ({ canvasElement }) => {
      await document.fonts.ready
      const markdown = requireElement<HTMLElement>(canvasElement, '[data-testid="markdown"]')
      const tails = markdown.querySelectorAll<HTMLElement>('[data-streaming-tail="true"]')
      if (tails.length !== 1) throw new Error(`Expected one streaming caret after ${terminalSelector}, found ${tails.length}`)
      const tail = tails[0]!
      const caret = getComputedStyle(tail, '::after')
      if (caret.content !== '""' || caret.display !== 'inline-block' || parseFloat(caret.width) <= 0 || parseFloat(caret.height) <= 0 || caret.backgroundColor === 'rgba(0, 0, 0, 0)') throw new Error('Streaming caret exists but is not painted')
      if (terminalSelector === 'p') {
        if (tail !== markdown.lastElementChild || tail.tagName !== 'P') throw new Error('Paragraph caret must stay inside the final paragraph, not on its own line')
        const range = document.createRange()
        range.selectNodeContents(tail)
        const lines = range.getClientRects()
        if (lines.length !== 1 || tail.getBoundingClientRect().height > parseFloat(getComputedStyle(tail).lineHeight) + 1) throw new Error('Paragraph caret introduced another line')
      } else if (tail !== markdown.lastElementChild || tail.tagName !== 'SPAN' || !tail.previousElementSibling?.matches(terminalSelector)) throw new Error(`Streaming fallback must follow the terminal ${terminalSelector}`)
    },
  }
}
export const StreamingCodeBlock: Story = streamingTailStory('The current implementation is:\n\n```typescript\nexport const selected = "row-3";\n```', '[data-testid="markdown-code-block"]')
export const StreamingHeading: Story = streamingTailStory('The next section is:\n\n## Selection details', 'h2')
export const StreamingTable: Story = streamingTailStory('Current resources:\n\n| Resource | State |\n| --- | --- |\n| Selection model | Ready |', '[role="region"]')
export const StreamingTightList: Story = streamingTailStory('Current checks:\n\n- Preserve selection identity.\n- Restore keyboard focus.', 'ul')
export const StreamingParagraph: Story = streamingTailStory('Selection identity stays stable.\n\nThe current result is ready.\n', 'p')
export const StreamingCodeBlockLight: Story = { ...StreamingCodeBlock, args: { scheme: 'light' } }
export const StreamingHeadingLight: Story = { ...StreamingHeading, args: { scheme: 'light' } }
export const StreamingTableLight: Story = { ...StreamingTable, args: { scheme: 'light' } }
export const StreamingTightListLight: Story = { ...StreamingTightList, args: { scheme: 'light' } }
export const StreamingParagraphLight: Story = { ...StreamingParagraph, args: { scheme: 'light' } }
export const RichText: Story = {
  play: async ({ canvasElement }) => {
    const root = requireElement(canvasElement, '[data-testid="markdown"]')
    const title = requireElement<HTMLAnchorElement>(root, 'h1 a')
    if (title.textContent !== 'Selection and ordering' || title.title !== 'Selection guide' || !title.getAttribute('href')?.includes('selection_(stable)')) throw new Error('Linked heading lost its formatted label, balanced URL, or title')
    if (root.querySelectorAll('strong').length < 4 || root.querySelectorAll('em').length < 4) throw new Error('Bold and italic markup did not produce semantic elements')
    if (root.querySelector('ol[start="3"] > li > ul > li > ol') === null) throw new Error('Nested mixed lists lost their structure or ordered start')
    if (root.querySelectorAll('table tbody tr').length !== 3 || !root.textContent?.includes('Escaped | pipe')) throw new Error('GFM table parsing leaked delimiters or dropped rows')
    if (root.querySelector('[data-testid="custom-inline-reference"]') === null) throw new Error('Custom inline reference renderer was lost')
    const resource = [...root.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent === 'src/model.ts')
    if (resource === undefined) throw new Error('Resource chip was lost')
    resource.click()
    await settle()
    if (canvasElement.querySelector('[data-testid="opened-resource"]')?.textContent !== 'Opened src/model.ts') throw new Error('Resource chip lost its open callback')
  },
}
export const Languages: Story = {
  args: { scenario: 'languages' },
  play: async ({ canvasElement }) => {
    const fences = [...canvasElement.querySelectorAll<HTMLElement>('[data-testid="markdown-code-block"]')]
    if (fences.length !== languages.length) throw new Error('Language coverage lost a fence')
    for (let index = 0; index < languages.length; index++) {
      const [language, code] = languages[index]!
      const fence = fences[index]!
      if (fence.dataset.language !== (language || 'text')) throw new Error(`Fence ${index} lost its language label`)
      if (index < languages.length - 2) {
        for (let frame = 0; fence.querySelector('[data-syntax-token]') === null && frame < 600; frame++) await settle()
        if (fence.querySelector('[data-syntax-token]') === null) throw new Error(`${language} did not produce real syntax tokens`)
      } else if (fence.querySelector('[data-syntax-token], script') !== null) throw new Error('Unknown language should remain safe literal text')
      if (fence.querySelector('code')?.textContent !== `${code}\n`) throw new Error(`${language || 'text'} highlighting changed the source text`)
    }
  },
}
export const Light: Story = { ...RichText, args: { scheme: 'light' } }
export const AllStates: Story = { args: { scenario: 'all' } }
export const LanguagesLight: Story = { ...Languages, args: { scenario: 'languages', scheme: 'light' } }
export const AllStatesLight: Story = { args: { scenario: 'all', scheme: 'light' } }
export const StreamingControlsPersistence: Story = {
  args: { scenario: 'streaming' },
  play: async ({ canvasElement }) => {
    await document.fonts.ready
    await settle()
    const fence = requireElement<HTMLElement>(canvasElement, '[data-testid="markdown-code-block"]')
    const wrap = requireElement<HTMLButtonElement>(fence, '[aria-label="Wrap code"]')
    const copy = requireElement<HTMLButtonElement>(fence, '[aria-label="Copy code"]')
    const append = requireElement<HTMLButtonElement>(canvasElement, '[data-testid="append-stream"]')
    for (let frame = 0; fence.querySelector('[data-syntax-token]') === null && frame < 600; frame++) await settle()
    if (fence.querySelector('[data-syntax-token]') === null) throw new Error('Lazy TypeScript grammar did not finish loading before the interaction')
    const clipboardDescriptor = Object.getOwnPropertyDescriptor(navigator, 'clipboard')
    const copied: string[] = []
    // Deterministic clipboard transport for the story only; production uses the real Clipboard API.
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async (value: string) => { copied.push(value) } } })
    try {
      flushSync(() => wrap.click())
      await settle()
      if (wrap.getAttribute('aria-pressed') !== 'true') throw new Error('Wrap toggle did not enable wrapping')
      flushSync(() => copy.click())
      await settle()
      if (fence.dataset.copyState !== 'copied' || copied[0] !== fence.querySelector('code')?.textContent) throw new Error('Copy did not write the raw rendered code')
      for (let stage = 1; stage < streamChunks.length; stage++) {
        flushSync(() => append.click())
        await settle()
        const current = requireElement<HTMLElement>(canvasElement, '[data-testid="markdown-code-block"]')
        if (current !== fence || current.dataset.wrap !== 'true' || current.dataset.copyState !== 'copied' || current.querySelector('[aria-label="Wrap code"]') !== wrap || current.querySelector('[aria-label="Copy code"]') !== copy) throw new Error(`Streaming chunk ${stage} remounted the fence or reset local controls`)
        if (copy.textContent !== 'Copy') throw new Error('Streaming code change falsely retained a current-version Copied label')
      }
      flushSync(() => copy.click())
      await settle()
      if (copied.at(-1) !== fence.querySelector('code')?.textContent || copy.textContent !== 'Copied') throw new Error('Copy after streaming did not use the latest code')
      const second = canvasElement.querySelectorAll<HTMLElement>('[data-testid="markdown-code-block"]')[1]
      if (second?.dataset.wrap !== 'false') throw new Error('A new fence inherited another fence’s local wrap state')
    } finally {
      if (clipboardDescriptor === undefined) Reflect.deleteProperty(navigator, 'clipboard')
      else Object.defineProperty(navigator, 'clipboard', clipboardDescriptor)
    }
  },
}
export const StreamingControlsPersistenceLight: Story = { ...StreamingControlsPersistence, args: { scenario: 'streaming', scheme: 'light' } }
export const StreamingInlineMarkup: Story = {
  args: { scenario: 'inline-streaming' },
  play: async ({ canvasElement }) => {
    const adversarial = '['.repeat(50_000)
    const started = performance.now()
    const completed = completeStreamingTail(adversarial)
    const durationMs = performance.now() - started
    canvasElement.dataset.streamingTailDuration = String(durationMs)
    if (durationMs >= 50) throw new Error(`50k opening brackets exceeded the 50ms streaming-tail budget: ${durationMs}ms`)
    if (completed !== '['.repeat(49_999)) throw new Error('Streaming tail must remove only the last unmatched opening bracket')
    if (completeStreamingTail('Read [**label**](https://example.com') !== 'Read **label**') throw new Error('Incomplete destination did not retain its formatted label')
    if (completeStreamingTail('\\[literal') !== '\\[literal') throw new Error('Escaped opening bracket must remain literal')
    for (const [input, expected] of [
      ['Read [label](https://[::1', 'Read label'],
      ['Read [outer [inner]](https://[::1', 'Read outer [inner]'],
      ['Read [label](https://example.com/a(b)', 'Read label'],
      ['Read [label](https://example.com/a\\)b', 'Read label'],
      ['Read [label](https://example.com/a\\\\)', 'Read [label](https://example.com/a\\\\)'],
      ['Read [escaped \\[label\\]](https://[::1', 'Read escaped \\[label\\]'],
      ['Read \\[literal', 'Read \\[literal'],
    ] as const) if (completeStreamingTail(input) !== expected) throw new Error(`Streaming link-tail mismatch for ${JSON.stringify(input)}`)
    await document.fonts.ready
    const append = requireElement<HTMLButtonElement>(canvasElement, '[data-testid="append-stream"]')
    for (let stage = 0; stage < inlineStreamStages.length; stage++) {
      const markdown = requireElement<HTMLElement>(canvasElement, '[data-testid="markdown"]')
      if (markdown.textContent?.match(/[*`\[\]]/) || markdown.textContent?.includes('https://')) throw new Error(`Inline streaming stage ${stage} leaked raw markdown`)
      if (markdown.querySelector('strong')?.textContent !== 'Stable') throw new Error(`Inline streaming stage ${stage} lost bold semantics`)
      if (stage >= 1 && markdown.querySelector('em')?.textContent !== 'ordered') throw new Error('Partial italic lost semantic formatting')
      if (stage >= 2 && ![...markdown.querySelectorAll('button')].some(button => button.textContent === 'src/model.ts')) throw new Error('Partial inline code lost its resource chip')
      if (stage === inlineStreamStages.length - 1) {
        if (markdown.querySelector('a')?.getAttribute('href') !== 'https://example.com/guide') throw new Error('Completed streamed link did not become actionable')
        break
      }
      flushSync(() => append.click())
      await settle()
    }
  },
}

export const CopyFailure: Story = {
  args: { scenario: 'streaming' },
  play: async ({ canvasElement }) => {
    const fence = requireElement<HTMLElement>(canvasElement, '[data-testid="markdown-code-block"]')
    const copy = requireElement<HTMLButtonElement>(fence, '[aria-label="Copy code"]')
    await document.fonts.ready
    for (let frame = 0; fence.querySelector('[data-syntax-token]') === null && frame < 600; frame++) await settle()
    if (fence.querySelector('[data-syntax-token]') === null) throw new Error('Lazy grammar did not finish loading before the copy failure interaction')
    const descriptor = Object.getOwnPropertyDescriptor(navigator, 'clipboard')
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async () => { throw new DOMException('Clipboard permission denied', 'NotAllowedError') } } })
    try {
      flushSync(() => copy.click())
      await settle()
      if (fence.dataset.copyState !== 'failed' || !fence.querySelector('[role="status"]')?.textContent?.includes('Check clipboard permissions') || copy.disabled) throw new Error('Copy failure must report the permission problem and leave retry available')
    } finally {
      if (descriptor === undefined) Reflect.deleteProperty(navigator, 'clipboard')
      else Object.defineProperty(navigator, 'clipboard', descriptor)
    }
  },
}

const styles = stylex.create({
  canvas: { minHeight: '100vh', boxSizing: 'border-box', padding: s.xl, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  lane: { width: '100%', maxWidth: g.lane, minWidth: 0, marginInline: 'auto' },
  toolbar: { display: 'flex', alignItems: 'center', gap: s.md, marginBlockEnd: s.lg },
  button: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary }, ':disabled': { color: ink.fgMuted, cursor: 'default' } },
  heading: { marginBlock: s.lg, fontSize: t.headingSize, lineHeight: t.headingLeading },
})

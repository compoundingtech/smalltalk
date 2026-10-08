import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Link, Tooltip, TooltipTrigger, VisuallyHidden } from 'react-aria-components'
import ReactMarkdown, { defaultUrlTransform } from 'react-markdown'
import type { Components, ExtraProps } from 'react-markdown'
import type { Root, RootContent as SyntaxNode } from 'hast'
import { refractor } from 'refractor/core'
import typescript from 'refractor/typescript'
import tsx from 'refractor/tsx'
import javascript from 'refractor/javascript'
import json from 'refractor/json'
import bash from 'refractor/bash'
import diff from 'refractor/diff'
import rust from 'refractor/rust'
import nix from 'refractor/nix'
import python from 'refractor/python'
import yaml from 'refractor/yaml'
import markdown from 'refractor/markdown'
import css from 'refractor/css'
import remarkGfm from 'remark-gfm'
import { surfaceVars as surface, textVars as textColor, borderVars as border, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'

export interface InlineResource { readonly path: string; readonly added: number; readonly removed: number; readonly preview?: string }
export type InlineReferenceRenderer = (path: string) => React.ReactNode | undefined
export type MarkdownImageResolution = { readonly _tag: 'Load'; readonly src: string } | { readonly _tag: 'Defer' }
export type MarkdownImageResolver = (src: string) => MarkdownImageResolution
export const MarkdownImagePolicy = React.createContext<MarkdownImageResolver | undefined>(undefined)
export interface MarkdownProps { text: string; streaming?: boolean; resources?: readonly InlineResource[]; onOpenResource?: (path?: string) => void; renderInlineReference?: InlineReferenceRenderer; resolveImage?: MarkdownImageResolver }
const EMPTY_RESOURCES: readonly InlineResource[] = []
const REMARK_PLUGINS = [remarkGfm]
const STREAMING_PLUGINS = [markStreamingTail]
type ReferenceOptions = Omit<MarkdownProps, 'text' | 'streaming'>
const EMPTY_REFERENCES: ReferenceOptions = {}
const ReferenceContext = React.createContext<ReferenceOptions>(EMPTY_REFERENCES)
/** Paragraphs keep the caret on their final line; every other terminal block gets an inline fallback. */
function markStreamingTail() {
  return (tree: Root) => {
    for (let index = tree.children.length - 1; index >= 0; index--) {
      const last = tree.children[index]!
      if (last.type !== 'element') continue
      if (last.tagName === 'p') {
        last.properties['data-streaming-tail'] = true
        return
      }
      break
    }
    tree.children.push({ type: 'element', tagName: 'span', properties: { 'data-streaming-tail': true, 'aria-hidden': true }, children: [] })
  }
}

export const Markdown = React.memo(function Markdown({ text, streaming = false, resources = EMPTY_RESOURCES, onOpenResource, renderInlineReference, resolveImage }: MarkdownProps) {
  const inheritedImagePolicy = React.useContext(MarkdownImagePolicy)
  const references = React.useMemo(() => ({ resources, onOpenResource, renderInlineReference, resolveImage: resolveImage ?? inheritedImagePolicy }), [resources, onOpenResource, renderInlineReference, resolveImage, inheritedImagePolicy])
  const source = React.useMemo(() => streaming ? completeStreamingTail(text) : text, [text, streaming])
  return <ReferenceContext.Provider value={references}><div data-testid="markdown" {...stylex.props(styles.markdown)}><ReactMarkdown skipHtml urlTransform={(url, key) => key === 'src' ? url : defaultUrlTransform(url)} remarkPlugins={REMARK_PLUGINS} rehypePlugins={streaming ? STREAMING_PLUGINS : undefined} components={MARKDOWN_COMPONENTS}>{source}</ReactMarkdown></div></ReferenceContext.Provider>
})

function DeferredImage({ src = '', alt = '' }: React.ComponentProps<'img'>) {
  const { resolveImage } = React.useContext(ReferenceContext)
  const [approvedSrc, setApprovedSrc] = React.useState<string | undefined>(undefined)
  const policy = resolveImage?.(src) ?? { _tag: 'Defer' }
  const loadedSrc = policy._tag === 'Load' ? policy.src : approvedSrc === src ? src : undefined
  let host = 'image source'
  try { const url = new URL(src); host = url.host || `${url.protocol} image` } catch { host = 'attachment' }
  return loadedSrc !== undefined ? <img src={loadedSrc} alt={alt} referrerPolicy="no-referrer" {...stylex.props(styles.image)} /> : <span data-testid="deferred-image" data-image-src={src} {...stylex.props(styles.imagePlaceholder)}><span>{alt || 'Image'} · {host}</span><Button onPress={() => setApprovedSrc(src)} onClick={event => { event.preventDefault(); event.stopPropagation() }} {...stylex.props(styles.imageAction)}>Load image</Button></span>
}

function MarkdownLink({ children, href, title, node }: React.ComponentProps<'a'> & ExtraProps) {
  const { resolveImage } = React.useContext(ReferenceContext)
  const imageLink = node?.children.some(child => child.type === 'element' && child.tagName === 'img') ?? false
  const link = <Link href={href} target="_blank" rel="noopener noreferrer" render={props => <a {...props as React.ComponentPropsWithRef<'a'>} title={title} />} {...stylex.props(styles.link)}>{imageLink ? 'Open link' : children}</Link>
  return <ReferenceContext.Provider value={{ resolveImage }}>{imageLink ? <span>{children}{' '}{link}</span> : link}</ReferenceContext.Provider>
}

function Paragraph({ children, node }: React.ComponentProps<'p'> & ExtraProps) {
  const live = node?.properties['data-streaming-tail'] === true
  return <p data-streaming-tail={live || undefined} {...stylex.props(styles.paragraph, styles.prose, live && styles.streamingTail)}><InlineText>{children}</InlineText></p>
}

function renderReference(path: string, options: ReferenceOptions): React.ReactNode | undefined {
  const rendered = options.renderInlineReference?.(path)
  if (rendered !== undefined) return rendered
  const resource = options.resources?.find(file => file.path === path)
  return resource === undefined ? undefined : <ResourceChip resource={resource} onOpen={options.onOpenResource} />
}

/** Only text leaves are substituted: parsed emphasis, links, and list structure stay intact. */
function InlineText({ children }: { children: React.ReactNode }) {
  const references = React.useContext(ReferenceContext)
  return React.Children.map(children, child => {
    if (typeof child !== 'string') return child
    return child.split(/(\b(?:[\w.-]+\/)+[\w.-]+\.[\w]+\b)/g).map((part, index) => {
      const rendered = renderReference(part, references)
      return <React.Fragment key={index}>{rendered === undefined ? part : rendered}</React.Fragment>
    })
  })
}

function InlineCode({ children }: React.ComponentProps<'code'>) {
  const references = React.useContext(ReferenceContext)
  const path = String(children ?? '')
  const rendered = renderReference(path, references)
  return rendered === undefined ? <code {...stylex.props(styles.inlineCode)}>{children}</code> : rendered
}

// Eager, fixed package grammars: authored fences never trigger a module request.
const grammars = { typescript, tsx, javascript, json, bash, diff, rust, nix, python, yaml, markdown, css }
for (const grammar of Object.values(grammars)) refractor.register(grammar)
const languageAliases: Readonly<Record<string, keyof typeof grammars>> = {
  ts: 'typescript', js: 'javascript', sh: 'bash', shell: 'bash', shellscript: 'bash',
  rs: 'rust', py: 'python', yml: 'yaml', md: 'markdown',
}

function SyntaxToken({ node }: { node: SyntaxNode }): React.ReactNode {
  if (node.type === 'text') return node.value
  if (node.type !== 'element') return null
  const classes = Array.isArray(node.properties.className) ? node.properties.className.map(String) : []
  return <span data-syntax-token={classes.filter(value => value !== 'token').join(' ')} {...stylex.props(
    classes.some(value => ['comment', 'prolog', 'doctype', 'cdata'].includes(value)) && styles.syntaxComment,
    classes.some(value => ['keyword', 'atrule', 'tag', 'selector'].includes(value)) && styles.syntaxKeyword,
    classes.some(value => ['string', 'char', 'attr-value', 'inserted'].includes(value)) && styles.syntaxString,
    classes.some(value => ['number', 'boolean', 'constant', 'symbol'].includes(value)) && styles.syntaxLiteral,
    classes.some(value => ['function', 'class-name', 'builtin'].includes(value)) && styles.syntaxFunction,
    classes.includes('deleted') && styles.syntaxDeleted,
  )}>{node.children.map((child, index) => <SyntaxToken key={index} node={child} />)}</span>
}

export function HighlightedSource({ code, language }: { code: string; language: string }) {
  const normalized = language.toLowerCase()
  const canonical = Object.hasOwn(languageAliases, normalized) ? languageAliases[normalized]! : normalized
  const nodes = React.useMemo(() => Object.hasOwn(grammars, canonical) ? refractor.highlight(code, canonical).children : undefined, [code, canonical])
  return nodes === undefined ? code : <>{nodes.map((node, index) => <SyntaxToken key={index} node={node} />)}</>
}

type CopyState = { tag: 'ready' } | { tag: 'copying' } | { tag: 'copied'; code: string } | { tag: 'failed' }
const CodeBlock = React.memo(function CodeBlock({ code, language }: { code: string; language: string }) {
  const [wrap, setWrap] = React.useState(false)
  const [copy, setCopy] = React.useState<CopyState>({ tag: 'ready' })
  const copyCode = async () => {
    setCopy({ tag: 'copying' })
    try {
      await navigator.clipboard.writeText(code)
      setCopy({ tag: 'copied', code })
    } catch {
      setCopy({ tag: 'failed' })
    }
  }
  const copiedCurrentCode = copy.tag === 'copied' && copy.code === code
  return <div data-testid="markdown-code-block" data-language={language || 'text'} data-wrap={wrap} data-copy-state={copy.tag} {...stylex.props(styles.codeBlock)}>
    <div {...stylex.props(styles.codeToolbar)}>
      <span data-testid="markdown-code-language" {...stylex.props(styles.codeLanguage)}>{language || 'text'}</span>
      <Button aria-label="Wrap code" aria-pressed={wrap} onPress={() => setWrap(value => !value)} {...stylex.props(styles.codeButton, wrap && styles.codeButtonSelected)}>Wrap</Button>
      <Button aria-label="Copy code" isDisabled={copy.tag === 'copying'} onPress={copyCode} {...stylex.props(styles.codeButton)}>{copy.tag === 'copying' ? 'Copying…' : copiedCurrentCode ? 'Copied' : 'Copy'}</Button>
    </div>
    <pre data-testid="markdown-code" {...stylex.props(styles.codePre, wrap && styles.codeWrapped)}><code {...stylex.props(styles.codeText)}><HighlightedSource code={code} language={language} /></code></pre>
    <span role="status" {...stylex.props(copy.tag === 'failed' && styles.copyStatus)}>{copy.tag === 'failed' ? 'Could not copy code. Check clipboard permissions and try again.' : <VisuallyHidden><span>{copiedCurrentCode ? 'Code copied.' : copy.tag === 'copied' ? 'Code has changed since the last copy.' : ''}</span></VisuallyHidden>}</span>
  </div>
})

function CodeFence({ node }: React.ComponentProps<'pre'> & ExtraProps) {
  const codeNode = node?.children.find(child => child.type === 'element' && child.tagName === 'code')
  if (codeNode?.type !== 'element') return null
  const code = codeNode.children.map(child => child.type === 'text' ? child.value : '').join('')
  const classes = codeNode.properties.className
  const language = /(?:^|\s)language-([^\s]+)/.exec(Array.isArray(classes) ? classes.join(' ') : String(classes ?? ''))?.[1] ?? ''
  return <CodeBlock code={code} language={language} />
}

const heading = (Tag: 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6') => function Heading({ children }: React.ComponentProps<'h3'>) {
  return <Tag {...stylex.props(styles.heading, styles.prose)}><InlineText>{children}</InlineText></Tag>
}
const MARKDOWN_COMPONENTS: Components = {
  h1: heading('h1'), h2: heading('h2'), h3: heading('h3'), h4: heading('h4'), h5: heading('h5'), h6: heading('h6'),
  p: Paragraph,
  span: ({ children, node }) => <span data-streaming-tail={node?.properties['data-streaming-tail'] || undefined} aria-hidden={node?.properties['aria-hidden'] === true || undefined} {...stylex.props(node?.properties['data-streaming-tail'] === true && styles.streamingTail)}>{children}</span>,
  strong: ({ children }) => <strong {...stylex.props(styles.strong)}><InlineText>{children}</InlineText></strong>,
  em: ({ children }) => <em {...stylex.props(styles.emphasis)}><InlineText>{children}</InlineText></em>,
  ul: ({ children }) => <ul {...stylex.props(styles.list, styles.prose, styles.unorderedList)}>{children}</ul>,
  ol: ({ children, start }) => <ol start={start} {...stylex.props(styles.list, styles.prose, styles.orderedList)}>{children}</ol>,
  li: ({ children }) => <li {...stylex.props(styles.listItem)}><InlineText>{children}</InlineText></li>,
  a: MarkdownLink,
  code: InlineCode,
  img: DeferredImage,
  pre: CodeFence,
  blockquote: ({ children }) => <blockquote {...stylex.props(styles.blockquote)}>{children}</blockquote>,
  table: ({ children }) => <div role="region" aria-label="Markdown table" tabIndex={0} {...stylex.props(styles.tableScroll)}><table {...stylex.props(styles.table)}>{children}</table></div>,
  th: ({ children, style }) => <th {...stylex.props(styles.cell, styles.tableHeading, style?.textAlign === 'center' ? styles.alignCenter : style?.textAlign === 'right' ? styles.alignRight : styles.alignLeft)}><InlineText>{children}</InlineText></th>,
  td: ({ children, style }) => <td {...stylex.props(styles.cell, style?.textAlign === 'center' ? styles.alignCenter : style?.textAlign === 'right' ? styles.alignRight : styles.alignLeft)}><InlineText>{children}</InlineText></td>,
}
export function ResourceChip({ resource, onOpen }: { resource: InlineResource; onOpen?: (path?: string) => void }) {
  return <TooltipTrigger delay={300}><Button onPress={() => onOpen?.(resource.path)} {...stylex.props(styles.resource)}>{resource.path}</Button><Tooltip {...stylex.props(styles.hoverCard)}><div {...stylex.props(styles.resourceTitle)}>{resource.path}<span {...stylex.props(styles.added)}>+{resource.added}</span><span {...stylex.props(styles.removed)}>−{resource.removed}</span></div>{resource.preview ? <pre {...stylex.props(styles.preview)}>{resource.preview}</pre> : null}</Tooltip></TooltipTrigger>
}
/** Complete only the unfinished prose tail; fenced code and settled literal punctuation are untouched. */
export function completeStreamingTail(source: string): string {
  let fence: { marker: string; length: number } | undefined
  let tailStart = 0
  let offset = 0
  for (const line of source.split('\n')) {
    const marker = /^ {0,3}(`{3,}|~{3,})/.exec(line)?.[1]
    if (marker !== undefined) {
      if (fence === undefined) fence = { marker: marker[0]!, length: marker.length }
      else if (marker[0] === fence.marker && marker.length >= fence.length && line.trim() === marker) { fence = undefined; tailStart = offset + line.length + 1 }
    } else if (fence === undefined && line.trim() === '') tailStart = offset + line.length + 1
    offset += line.length + 1
  }
  if (fence !== undefined) return source
  let tail = source.slice(tailStart)
  const delimiters: string[] = []
  let codeDelimiter: string | undefined
  for (let index = 0; index < tail.length; index++) {
    if (tail[index] === '\\') { index += 1; continue }
    const character = tail[index]!
    if (character === '`') {
      const run = /^`+/.exec(tail.slice(index))![0]
      if (codeDelimiter === undefined && index + run.length === tail.length) { tail = tail.slice(0, index); break }
      if (codeDelimiter === undefined) codeDelimiter = run
      else if (codeDelimiter === run) codeDelimiter = undefined
      index += run.length - 1
      continue
    }
    if (codeDelimiter !== undefined || (character !== '*' && character !== '_')) continue
    const run = character === '*' ? /^\*+/.exec(tail.slice(index))![0] : /^_+/.exec(tail.slice(index))![0]
    const before = tail[index - 1] ?? ' '
    const after = tail[index + run.length] ?? ' '
    const beforePunctuation = /[\p{P}\p{S}]/u.test(before)
    const afterPunctuation = /[\p{P}\p{S}]/u.test(after)
    const opens = !/\s/.test(after) && (!afterPunctuation || /\s/.test(before) || beforePunctuation)
    const closes = !/\s/.test(before) && (!beforePunctuation || /\s/.test(after) || afterPunctuation)
    let remaining = run.length
    if (closes && (character !== '_' || !opens || afterPunctuation)) {
      while (remaining > 0 && delimiters.at(-1)?.startsWith(character)) {
        const pending = delimiters.pop()!
        const consumed = Math.min(remaining, pending.length)
        remaining -= consumed
        if (consumed < pending.length) delimiters.push(pending.slice(consumed))
      }
    }
    if (remaining > 0 && opens && (character !== '_' || !closes || beforePunctuation)) delimiters.push(character.repeat(remaining))
    else if (!opens && !closes && index + run.length === tail.length) { tail = tail.slice(0, index); break }
    index += run.length - 1
  }
  // Until a destination is complete, show its parsed label rather than a raw partial URL.
  // Scan the last line backward once; consume escape runs once instead of retrying a regex at every '['.
  if (codeDelimiter === undefined) {
    let closing = -1
    let parentheses = 0
    // Locate the label boundary before examining brackets in a possibly unfinished destination.
    for (let index = tail.length - 1; index >= 0 && tail[index] !== '\n'; index--) {
      const character = tail[index]
      if (character !== '(' && character !== ')') continue
      const parenthesisIndex = index
      while (index > 0 && tail[index - 1] === '\\') index--
      if ((parenthesisIndex - index) % 2 === 1) continue
      if (character === ')') parentheses++
      else if (parentheses > 0) parentheses--
      else if (tail[parenthesisIndex - 1] === ']') {
        let escapeStart = parenthesisIndex - 1
        while (escapeStart > 0 && tail[escapeStart - 1] === '\\') escapeStart--
        if ((parenthesisIndex - 1 - escapeStart) % 2 === 0) { closing = parenthesisIndex - 1; break }
      }
    }
    let pendingClosings = 0
    for (let index = closing === -1 ? tail.length - 1 : closing; index >= 0 && tail[index] !== '\n'; index--) {
      const character = tail[index]
      if (character !== '[' && character !== ']') continue
      const bracketIndex = index
      while (index > 0 && tail[index - 1] === '\\') index--
      if ((bracketIndex - index) % 2 === 1) continue
      if (character === ']') pendingClosings++
      else if (pendingClosings === 0) {
        tail = tail.slice(0, bracketIndex) + tail.slice(bracketIndex + 1)
        break
      } else {
        pendingClosings--
        if (pendingClosings === 0 && (closing !== -1 || tail.endsWith(']'))) {
          const labelEnd = closing === -1 ? tail.length - 1 : closing
          tail = tail.slice(0, bracketIndex) + tail.slice(bracketIndex + 1, labelEnd)
          break
        }
      }
    }
  }
  return source.slice(0, tailStart) + tail + (codeDelimiter ?? '') + delimiters.reverse().join('')
}

const styles = stylex.create({
  markdown: { display: 'flow-root', minWidth: 0, maxWidth: '100%' },
  image: { maxWidth: '100%', height: 'auto' },
  imagePlaceholder: { display: 'inline-flex', alignItems: 'center', gap: s.sm, padding: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.sm, color: textColor.fgMuted, backgroundColor: surface.controlFill, fontSize: t.metaSize },
  imageAction: { borderWidth: 0, padding: s.xs, backgroundColor: surface.transparent, color: accent.primary, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  prose: { maxWidth: g.proseMax, fontSize: t.bodySize, lineHeight: t.bodyLeading },
  heading: { margin: 0, marginBlockStart: s.xxl, marginBlockEnd: s.md, fontSize: t.headingSize, lineHeight: t.headingLeading, fontWeight: t.weightSemibold, color: textColor.fg },
  list: { margin: 0, marginBlockEnd: s.proseGap, paddingInlineStart: s.xxl, listStylePosition: 'outside' },
  unorderedList: { listStyleType: 'disc' }, orderedList: { listStyleType: 'decimal' },
  listItem: { display: 'list-item', fontSize: t.bodySize, lineHeight: t.bodyLeading, color: textColor.fgSoft },
  strong: { fontWeight: t.weightBold, color: textColor.fg },
  emphasis: { fontStyle: 'italic' },
  syntaxComment: { color: textColor.fgMuted, fontStyle: 'italic' },
  syntaxKeyword: { color: status.runningFg },
  syntaxString: { color: status.diffAdded },
  syntaxLiteral: { color: status.attention },
  syntaxFunction: { color: textColor.fg, fontWeight: t.weightMedium },
  syntaxDeleted: { color: status.dangerFg },
  blockquote: { margin: 0, marginBlockEnd: s.proseGap, paddingInlineStart: s.lg, borderInlineStartWidth: g.focusRing, borderInlineStartStyle: 'solid', borderInlineStartColor: border.borderStrong },
  tableScroll: { maxWidth: '100%', overflowX: 'auto', marginBlockEnd: s.proseGap, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  table: { width: '100%', borderCollapse: 'collapse', fontSize: t.bodySize, lineHeight: t.bodyLeading, color: textColor.fgSoft },
  cell: { paddingInline: s.md, paddingBlock: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, verticalAlign: 'top' },
  tableHeading: { backgroundColor: surface.raised, fontWeight: t.weightSemibold, color: textColor.fg },
  alignLeft: { textAlign: 'left' }, alignCenter: { textAlign: 'center' }, alignRight: { textAlign: 'right' },
  codeToolbar: { display: 'flex', alignItems: 'center', gap: s.xs, paddingInline: s.md, paddingBlock: s.xs, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  codeLanguage: { flexGrow: 1, minWidth: 0, overflowWrap: 'anywhere', fontFamily: t.fontMono, fontSize: t.metaSize, color: textColor.fgMuted },
  codeButton: { flexShrink: 0, minHeight: g.controlSm, paddingInline: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: surface.transparent, borderRadius: r.sm, backgroundColor: surface.transparent, color: textColor.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset }, ':disabled': { cursor: 'default', color: textColor.fgMuted } },
  codeButtonSelected: { backgroundColor: surface.rowActive, borderColor: border.borderStrong, color: textColor.fg },
  codePre: { margin: 0, padding: s.md, overflowX: 'auto', whiteSpace: 'pre' },
  codeWrapped: { whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
  copyStatus: { display: 'block', paddingInline: s.md, paddingBlock: s.xs, color: status.dangerFg, fontSize: t.metaSize, lineHeight: t.metaLeading },
  codeBlock: { minWidth: 0, maxWidth: '100%', margin: 0, marginBlockEnd: s.proseGap, backgroundColor: surface.codeBg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.md, overflow: 'hidden' },
  codeText: { fontFamily: t.fontMono, fontSize: t.codeSize, lineHeight: t.uiLeading, color: textColor.fgSoft },
  paragraph: { margin: 0, marginBlockEnd: s.proseGap, fontSize: t.bodySize, lineHeight: t.bodyLeading, color: textColor.fgSoft, overflowWrap: 'anywhere' },
  streamingTail: { '::after': { content: '""', display: 'inline-block', width: g.caret, height: t.bodySize, marginInlineStart: s.xs, verticalAlign: 'text-bottom', backgroundColor: textColor.fgMuted } },
  inlineCode: { fontFamily: t.fontMono, fontSize: t.metaSize, color: textColor.fg, backgroundColor: surface.codeBg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.sm, paddingInline: s.xs },
  link: { color: status.runningFg, textDecorationLine: 'none', ':hover': { textDecorationLine: 'underline' }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  resource: { display: 'inline', fontFamily: t.fontMono, fontSize: t.metaSize, color: textColor.fg, backgroundColor: surface.controlFill, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, paddingInline: s.xs, paddingBlock: 0, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.hairline } }, hoverCard: { maxWidth: g.tooltipMax, padding: s.lg, borderRadius: r.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.raised, color: textColor.fg, fontSize: t.metaSize, lineHeight: t.metaLeading }, resourceTitle: { display: 'flex', alignItems: 'center', gap: s.md, fontFamily: t.fontMono }, added: { color: status.diffAdded }, removed: { color: status.diffRemoved }, preview: { margin: 0, marginTop: s.md, fontFamily: t.fontMono, fontSize: t.denseSize, lineHeight: t.metaLeading, color: textColor.fgMuted, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
})

import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, fn, userEvent, waitFor, within } from 'storybook/test'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { EmbraceThread } from './assistant-ui/EmbraceThread'
import { EmbraceMarkdownPreview } from './assistant-ui/EmbraceToolPreview'
import { Transcript } from './assistant-ui/composition/Transcript'
import type { MarkdownImageOpener, MarkdownImageResolver } from './assistant-ui/composition/Markdown'
import type { ConversationItem } from './assistant-ui/embrace-data/model'
import { workLogTurnFromItems } from './assistant-ui/taste/work-log'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { darkTheme as embraceDarkTheme } from './assistant-ui/embrace-theme'
import { surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'
/** A served local image decodes in milliseconds when idle; under a saturated strict gate it can exceed waitFor's 1s default, so readiness waits longer without weakening the decode assertion. */
const imageReadyTimeout = 10_000

const pixel = 'data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7'
const attachment = '/attachments/allowed.svg'
const sources = {
  Remote: 'https://example.invalid/x.png',
  Loopback: 'http://127.0.0.1/x.png',
  Metadata: 'http://169.254.169.254/',
  Data: pixel,
  Local: attachment,
  Linked: 'https://example.invalid/linked.png',
} as const
const at = '2026-01-15T12:00:00Z'
type Boundary = 'Transcript' | 'Embrace' | 'ToolPreview'
const openImage = fn<MarkdownImageOpener>()
const consentImage = fn<MarkdownImageOpener>()
function ContentNetworkStory({ boundary, source, allow = false, hostOpens = false, resolveOnConsent = false }: { boundary: Boundary; source: string; allow?: boolean; hostOpens?: boolean; resolveOnConsent?: boolean }) {
  const [approvedSource, setApprovedSource] = React.useState<string>()
  const text = `${source.endsWith('/linked.png') ? `[![Network image](${source})](https://example.invalid/image-link)` : `![Network image](${source})`}\n\n![Other image](https://example.invalid/other.png)\n\n<https://example.invalid/autolink>\n\n<img src="https://example.invalid/raw.png" srcset="https://example.invalid/raw2.png 2x" />\n\n<style>body { background-image: url(https://example.invalid/css.png) }</style>`
  const items = React.useMemo<readonly ConversationItem[]>(() => [{ _tag: 'Text', id: 'network/answer', role: 'assistant', text, attachments: [], streaming: false, at }], [text])
  const options = React.useMemo(() => ({ messages: items, isRunning: false, onNew: async () => {} }), [items])
  // A host can resolve an explicitly requested remote image to a local attachment.
  // Default inline consent remains covered separately by the local and data stories.
  const resolveImage: MarkdownImageResolver | undefined = allow || resolveOnConsent ? src => (allow && src === attachment) || (resolveOnConsent && src === approvedSource) ? { _tag: 'Load', src: attachment } : { _tag: 'Defer' } : undefined
  const work = workLogTurnFromItems(items, { kindFor: () => 'read', running: false, failed: false, interrupted: false, completeHistory: true })
  const onLoadImage = hostOpens ? openImage : resolveOnConsent ? (src: string) => { consentImage(src); setApprovedSource(src) } : undefined
  return <main {...stylex.props(styles.root, ...baselineTheme, embraceDarkTheme)}><EmbraceRuntimeProvider options={options}>{boundary === 'ToolPreview' ? <EmbraceMarkdownPreview markdown={text} resolveImage={resolveImage} onLoadImage={onLoadImage} /> : boundary === 'Embrace' ? <EmbraceThread items={items} embrace="E3" composer={false} resolveImage={resolveImage} onLoadImage={onLoadImage} /> : <Transcript turns={[{ id: 'network', items, work }]} title="Image privacy" sync={{ _tag: 'Live', since: 0 }} now={0} observedAt={0} resolveImage={resolveImage} onLoadImage={onLoadImage} />}</EmbraceRuntimeProvider></main>
}
const meta = { title: 'Fractal UI/Content network safety', component: ContentNetworkStory, parameters: { layout: 'fullscreen' } } satisfies Meta<typeof ContentNetworkStory>
export default meta
type Story = StoryObj<typeof meta>
const deferredStory = (boundary: Boundary, source: string): Story => ({
  args: { boundary, source, resolveOnConsent: source !== sources.Data && source !== sources.Local },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    consentImage.mockClear()
    const resolvedSource = source === sources.Data || source === sources.Local ? source : attachment
    const fixtureUrl = new URL(attachment, window.location.href).href
    const fixtureRequestsBefore = performance.getEntriesByType('resource').filter(entry => entry.name === fixtureUrl).length
    const placeholders = await canvas.findAllByTestId('deferred-image')
    await expect(placeholders).toHaveLength(2)
    await expect(placeholders[0]).toHaveAttribute('data-image-src', source)
    await expect(canvasElement.querySelector('img, iframe, style, [srcset]')).toBeNull()
    const links = canvasElement.querySelectorAll('a')
    for (const link of links) {
      await expect(link).toHaveAttribute('rel', 'noopener noreferrer')
      await expect(link).toHaveAttribute('target', '_blank')
    }
    const contentUrl = /^(?:https:\/\/example\.invalid\/|http:\/\/127\.0\.0\.1\/|http:\/\/169\.254\.169\.254\/)/
    await expect(performance.getEntriesByType('resource').filter(entry => contentUrl.test(entry.name))).toHaveLength(0)
    const hostResolved = source !== sources.Data && source !== sources.Local
    await userEvent.click(within(placeholders[0]!).getByRole('button', { name: hostResolved ? `Open image · ${new URL(source).host}` : 'Load image' }))
    if (hostResolved) {
      await expect(consentImage).toHaveBeenCalledTimes(1)
      await expect(consentImage).toHaveBeenCalledWith(source)
    } else {
      await expect(consentImage).not.toHaveBeenCalled()
    }
    await waitFor(() => expect(canvasElement.querySelectorAll('img')).toHaveLength(1))
    await expect(canvasElement.querySelector('img')).toHaveAttribute('src', resolvedSource)
    await expect(canvasElement.querySelector('img')).toHaveAttribute('referrerpolicy', 'no-referrer')
    await expect(canvas.getAllByTestId('deferred-image')).toHaveLength(1)
    const image = canvasElement.querySelector('img')!
    await waitFor(async () => { await expect(image.complete).toBe(true); await expect(image.naturalWidth).toBeGreaterThan(0) }, { timeout: imageReadyTimeout })
    await expect(performance.getEntriesByType('resource').filter(entry => contentUrl.test(entry.name))).toHaveLength(0)
    await expect(performance.getEntriesByType('resource').filter(entry => entry.name === fixtureUrl)).toHaveLength(fixtureRequestsBefore + (source === sources.Data ? 0 : 1))
  },
})
export const TranscriptRemote: Story = { ...deferredStory('Transcript', sources.Remote), tags: ['critical'] }
export const TranscriptLoopback: Story = { ...deferredStory('Transcript', sources.Loopback), tags: ['critical'] }
export const TranscriptMetadata: Story = { ...deferredStory('Transcript', sources.Metadata), tags: ['critical'] }
export const TranscriptData: Story = { ...deferredStory('Transcript', sources.Data), tags: ['critical'] }
export const TranscriptLinked: Story = { ...deferredStory('Transcript', sources.Linked), tags: ['critical'] }
export const TranscriptLocal: Story = { ...deferredStory('Transcript', sources.Local), tags: ['critical'] }
export const EmbraceRemote: Story = deferredStory('Embrace', sources.Remote)
export const EmbraceLoopback: Story = deferredStory('Embrace', sources.Loopback)
export const EmbraceMetadata: Story = deferredStory('Embrace', sources.Metadata)
export const EmbraceData: Story = deferredStory('Embrace', sources.Data)
export const EmbraceLinked: Story = deferredStory('Embrace', sources.Linked)
export const EmbraceLocal: Story = deferredStory('Embrace', sources.Local)
export const ToolPreviewRemote: Story = deferredStory('ToolPreview', sources.Remote)
export const ToolPreviewLoopback: Story = deferredStory('ToolPreview', sources.Loopback)
export const ToolPreviewMetadata: Story = deferredStory('ToolPreview', sources.Metadata)
export const ToolPreviewData: Story = deferredStory('ToolPreview', sources.Data)
export const ToolPreviewLinked: Story = deferredStory('ToolPreview', sources.Linked)
export const ToolPreviewLocal: Story = deferredStory('ToolPreview', sources.Local)
const allowedStory = (boundary: Boundary): Story => ({ args: { boundary, source: attachment, allow: true }, play: async ({ canvasElement }) => {
  await waitFor(() => expect(canvasElement.querySelectorAll('img')).toHaveLength(1))
  const image = canvasElement.querySelector('img')!
  await expect(image).toHaveAttribute('src', attachment)
  await expect(image).toHaveAttribute('referrerpolicy', 'no-referrer')
  await waitFor(async () => { await expect(image.complete).toBe(true); await expect(image.naturalWidth).toBeGreaterThan(0) }, { timeout: imageReadyTimeout })
  await expect(within(canvasElement).getAllByTestId('deferred-image')).toHaveLength(1)
  const contentUrl = /^(?:https:\/\/example\.invalid\/|http:\/\/127\.0\.0\.1\/|http:\/\/169\.254\.169\.254\/)/
  await expect(performance.getEntriesByType('resource').filter(entry => contentUrl.test(entry.name))).toHaveLength(0)
} })
export const TranscriptAllowedAttachment: Story = { ...allowedStory('Transcript'), tags: ['critical'] }
export const EmbraceAllowedAttachment: Story = allowedStory('Embrace')
export const ToolPreviewAllowedAttachment: Story = allowedStory('ToolPreview')
/** CSP hosts (img-src 'self' data: blob:) take the source and open it outside the page; the kit never fetches it. */
const hostOpensStory = (boundary: Boundary, source: string): Story => ({ args: { boundary, source, hostOpens: true }, play: async ({ canvasElement }) => {
  openImage.mockClear()
  const canvas = within(canvasElement)
  const placeholders = await canvas.findAllByTestId('deferred-image')
  await expect(placeholders).toHaveLength(2)
  const contentUrl = /^(?:https:\/\/example\.invalid\/|http:\/\/127\.0\.0\.1\/|http:\/\/169\.254\.169\.254\/)/
  await expect(performance.getEntriesByType('resource').filter(entry => contentUrl.test(entry.name))).toHaveLength(0)
  const host = new URL(source).host
  await expect(within(placeholders[0]!).queryByRole('button', { name: 'Load image' })).toBeNull()
  await userEvent.click(within(placeholders[0]!).getByRole('button', { name: `Open image · ${host}` }))
  await expect(openImage).toHaveBeenCalledTimes(1)
  await expect(openImage).toHaveBeenCalledWith(source)
  // The package lib is ES2022 (no Promise.withResolvers): give any stray image request time to appear.
  await new Promise(resolve => setTimeout(resolve, 500))
  await expect(canvasElement.querySelector('img, iframe, style, [srcset]')).toBeNull()
  await expect(canvas.getAllByTestId('deferred-image')).toHaveLength(2)
  await expect(performance.getEntriesByType('resource').filter(entry => contentUrl.test(entry.name))).toHaveLength(0)
} })
export const TranscriptHostOpensRemote: Story = { ...hostOpensStory('Transcript', sources.Remote), tags: ['critical'] }
export const TranscriptHostOpensLoopback: Story = { ...hostOpensStory('Transcript', sources.Loopback), tags: ['critical'] }
export const EmbraceHostOpensRemote: Story = hostOpensStory('Embrace', sources.Remote)
export const EmbraceHostOpensMetadata: Story = hostOpensStory('Embrace', sources.Metadata)
export const ToolPreviewHostOpensRemote: Story = hostOpensStory('ToolPreview', sources.Remote)
export const ToolPreviewHostOpensLinked: Story = hostOpensStory('ToolPreview', sources.Linked)
const styles = stylex.create({ root: { height: '100vh', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans } })

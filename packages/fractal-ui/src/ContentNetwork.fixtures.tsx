import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { StoryObj } from '@storybook/react-vite'
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

const pixel = 'data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7'
const sources = {
  Remote: 'https://example.invalid/x.png',
  Loopback: 'http://127.0.0.1/x.png',
  Metadata: 'http://169.254.169.254/',
  Data: pixel,
  Linked: 'https://example.invalid/linked.png',
} as const
const at = '2026-01-15T12:00:00Z'
type Boundary = 'Transcript' | 'Embrace' | 'ToolPreview'
const openImage = fn<MarkdownImageOpener>()
function ContentNetworkStory({ boundary, source, allow = false, hostOpens = false }: { boundary: Boundary; source: string; allow?: boolean; hostOpens?: boolean }) {
  const text = `${source.endsWith('/linked.png') ? `[![Network image](${source})](https://example.invalid/image-link)` : `![Network image](${source})`}\n\n![Other image](https://example.invalid/other.png)\n\n<https://example.invalid/autolink>\n\n<img src="https://example.invalid/raw.png" srcset="https://example.invalid/raw2.png 2x" />\n\n<style>body { background-image: url(https://example.invalid/css.png) }</style>`
  const items = React.useMemo<readonly ConversationItem[]>(() => [{ _tag: 'Text', id: 'network/answer', role: 'assistant', text, attachments: [], streaming: false, at }], [text])
  const options = React.useMemo(() => ({ messages: items, isRunning: false, onNew: async () => {} }), [items])
  const resolveImage: MarkdownImageResolver | undefined = allow ? src => src === '/attachments/allowed.png' ? { _tag: 'Load', src } : { _tag: 'Defer' } : undefined
  const work = workLogTurnFromItems(items, { kindFor: () => 'read', running: false, failed: false, interrupted: false, completeHistory: true })
  const onLoadImage = hostOpens ? openImage : undefined
  return <main {...stylex.props(styles.root, ...baselineTheme, embraceDarkTheme)}><EmbraceRuntimeProvider options={options}>{boundary === 'ToolPreview' ? <EmbraceMarkdownPreview markdown={text} resolveImage={resolveImage} onLoadImage={onLoadImage} /> : boundary === 'Embrace' ? <EmbraceThread items={items} embrace="E3" composer={false} resolveImage={resolveImage} onLoadImage={onLoadImage} /> : <Transcript turns={[{ id: 'network', items, work }]} title="Image privacy" sync={{ _tag: 'Live', since: 0 }} now={0} observedAt={0} resolveImage={resolveImage} onLoadImage={onLoadImage} />}</EmbraceRuntimeProvider></main>
}
type Story = StoryObj<typeof ContentNetworkStory>
const deferredStory = (boundary: Boundary, source: string): Story => ({
  args: { boundary, source },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    const placeholders = await canvas.findAllByTestId('deferred-image')
    await expect(placeholders).toHaveLength(2)
    await expect(canvasElement.querySelector('img, iframe, style, [srcset]')).toBeNull()
    const links = canvasElement.querySelectorAll('a')
    for (const link of links) {
      await expect(link).toHaveAttribute('rel', 'noopener noreferrer')
      await expect(link).toHaveAttribute('target', '_blank')
    }
    const contentUrl = /^(?:https:\/\/example\.invalid\/|http:\/\/127\.0\.0\.1\/|http:\/\/169\.254\.169\.254\/)/
    await expect(performance.getEntriesByType('resource').filter(entry => contentUrl.test(entry.name))).toHaveLength(0)
    await userEvent.click(within(placeholders[0]!).getByRole('button', { name: 'Load image' }))
    await waitFor(() => expect(canvasElement.querySelectorAll('img')).toHaveLength(1))
    await expect(canvasElement.querySelector('img')).toHaveAttribute('src', source)
    await expect(canvasElement.querySelector('img')).toHaveAttribute('referrerpolicy', 'no-referrer')
    await expect(canvas.getAllByTestId('deferred-image')).toHaveLength(1)
    const image = canvasElement.querySelector('img')!
    await waitFor(() => expect(image.complete).toBe(true))
    await expect(performance.getEntriesByType('resource').filter(entry => contentUrl.test(entry.name))).toHaveLength(source.startsWith('data:') ? 0 : 1)
  },
})
const allowedStory = (boundary: Boundary): Story => ({ args: { boundary, source: '/attachments/allowed.png', allow: true }, play: async ({ canvasElement }) => {
  await waitFor(() => expect(canvasElement.querySelectorAll('img')).toHaveLength(1))
  await expect(canvasElement.querySelector('img')).toHaveAttribute('src', '/attachments/allowed.png')
  await expect(within(canvasElement).getAllByTestId('deferred-image')).toHaveLength(1)
} })
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
const styles = stylex.create({ root: { height: '100vh', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans } })

export { ContentNetworkStory, sources, deferredStory, allowedStory, hostOpensStory }

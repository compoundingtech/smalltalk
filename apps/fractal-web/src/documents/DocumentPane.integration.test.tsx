// @vitest-environment jsdom
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { ClientError, type St3Client } from '@smalltalk/st3-client'

// One source object, as the app's data source is: the pane reads again only when it changes.
const data = vi.hoisted(() => { const read = vi.fn(); return { read, source: { documents: { read } } } })
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))
vi.mock('../data/react.tsx', () => ({ useDataSource: () => data.source }))

import { Markdown, MarkdownImagePolicy } from '../../../../packages/fractal-ui/src/assistant-ui/composition/Markdown.tsx'
import { DocumentPane } from './DocumentPane.tsx'
import { documentReferences } from './DocumentLink.tsx'
import { DOCUMENT_REF, documentVersion, gatewayDocuments } from './document-port.ts'

const hash = 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2'
const bytes = (text: string) => [...new TextEncoder().encode(text)]

describe('document references', () => {
  it('recognizes st document names and exact references, and nothing else', () => {
    expect(['doc/garden-notes', 'doc/example/web/checklist', `doc/garden-notes@${hash}`].every((ref) => DOCUMENT_REF.test(ref))).toBe(true)
    expect(['doc/', 'docs/readme.md', 'doc/a b', 'https://example.com/doc/x', 'doc/x@nothex'].some((ref) => DOCUMENT_REF.test(ref))).toBe(false)
    expect(documentVersion(`doc/garden-notes@${hash}`)).toEqual({ name: 'doc/garden-notes', version: 'a1b2c3d4e5f6' })
    expect(documentVersion('doc/garden-notes')).toEqual({ name: 'doc/garden-notes' })
  })

  it('lets a host make doc names in any Markdown below it open, through the transcript\'s policy', () => {
    const html = renderToStaticMarkup(
      <MarkdownImagePolicy.Provider value={{ renderInlineReference: documentReferences(() => {}) }}>
        <Markdown text="Read `doc/garden-notes` and `src/main.rs`." />
      </MarkdownImagePolicy.Provider>,
    )
    expect(html).toContain('<a href="doc/garden-notes">doc/garden-notes</a>')
    expect(html).toContain('<code>src/main.rs</code>')
  })
})

describe('gatewayDocuments', () => {
  it('reads text by name, says when it is not text, and says why it cannot be read', async () => {
    const documentGet = vi.fn()
      .mockResolvedValueOnce({ value: { reference: `doc/garden-notes@${hash}`, bytes: bytes('# Garden\n') } })
      .mockResolvedValueOnce({ value: { reference: `doc/photo@${hash}`, bytes: [0xff, 0xd8, 0xff] } })
      .mockRejectedValueOnce(new ClientError({ api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/x', code: 'not-found', message: 'no document', retryable: false, details: {} }, 404))
      .mockRejectedValueOnce(new TypeError('network down'))
    const port = gatewayDocuments({ documentGet } as unknown as St3Client)
    expect(await port.read('doc/garden-notes')).toEqual({ _tag: 'Text', reference: `doc/garden-notes@${hash}`, text: '# Garden\n' })
    expect(documentGet).toHaveBeenCalledWith('doc/garden-notes')
    expect(await port.read('doc/photo')).toEqual({ _tag: 'Binary', reference: `doc/photo@${hash}`, size: 3 })
    expect(await port.read('doc/missing')).toEqual({ _tag: 'Unavailable', reason: 'not-found', detail: 'There is no document doc/missing.' })
    expect(await port.read('doc/garden-notes')).toMatchObject({ _tag: 'Unavailable', reason: 'failed' })
  })
})

describe('DocumentPane', () => {
  it('reads the document, shows its version and Markdown, and opens the documents it links to', async () => {
    vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
    data.read.mockResolvedValue({
      _tag: 'Text',
      reference: `doc/garden-notes@${hash}`,
      text: '# Garden\n\n| bed | crop |\n| --- | --- |\n| north | beans |\n\nSee [the plan](doc/garden-plan) and `doc/seed-list`.\n',
    })
    const onOpen = vi.fn()
    const container = document.createElement('div')
    document.body.append(container)
    const root = createRoot(container)
    await act(async () => root.render(<DocumentPane name="doc/garden-notes" onOpen={onOpen} />))
    expect(data.read).toHaveBeenCalledWith('doc/garden-notes')
    const pane = container.querySelector('[data-testid="document-pane"]')!
    expect(pane.getAttribute('aria-label')).toBe('Document doc/garden-notes')
    expect(pane.textContent).toContain('version a1b2c3d4e5f6')
    expect(pane.querySelector('h1')?.textContent).toBe('Garden')
    expect(pane.querySelector('table td')?.textContent).toBe('north')
    await act(async () => (pane.querySelector('a[href="doc/garden-plan"]') as HTMLElement).click())
    await act(async () => (pane.querySelector('a[href="doc/seed-list"]') as HTMLElement).click())
    expect(onOpen.mock.calls).toEqual([['doc/garden-plan'], ['doc/seed-list']])

    data.read.mockResolvedValue({ _tag: 'Unavailable', reason: 'not-found', detail: 'There is no document doc/garden-notes.' })
    await act(async () => ([...pane.querySelectorAll('button')].find((button) => button.textContent === 'Read the newest') as HTMLElement).click())
    expect(data.read).toHaveBeenCalledTimes(2)
    expect(container.querySelector('[role="status"]')?.textContent).toBe('There is no document doc/garden-notes.')
    act(() => root.unmount())
    container.remove()
  })
})

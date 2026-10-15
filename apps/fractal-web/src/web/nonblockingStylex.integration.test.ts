// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { deferStylexDevTags } from '../../scripts/stylex.mjs'
import main from './main.tsx?raw'
import type { HtmlTagDescriptor } from 'vite'

const original: readonly HtmlTagDescriptor[] = [
  { tag: 'script', attrs: { type: 'module', src: '/@id/virtual:stylex:runtime' }, injectTo: 'head' },
  { tag: 'link', attrs: { rel: 'stylesheet', href: '/virtual:stylex.css' }, injectTo: 'head' },
]
afterEach(() => { document.head.innerHTML = ''; document.body.innerHTML = ''; delete window.__wfStylexReady; vi.useRealTimers() })
describe('critical shell independence from development CSS', () => {
  it.each(['timeout', 'error'] as const)('keeps a themed recovery action on %s and removes it when CSS eventually loads', failure => {
    vi.useFakeTimers()
    document.body.innerHTML = '<div data-wf-shell-skeleton aria-busy="true"><div data-wf-workspace-skeleton aria-hidden="true"></div></div>'
    const link = document.createElement('link')
    link.media = 'print'; link.dataset.wfStylexBootstrap = ''
    document.head.append(link)
    const bootstrap = deferStylexDevTags(original)?.[2]?.children
    expect(typeof bootstrap).toBe('string')
    if (typeof bootstrap === 'string') new Function('document', bootstrap)(document)
    if (failure === 'timeout') vi.advanceTimersByTime(5000)
    else link.dispatchEvent(new Event('error'))
    expect(document.querySelector('[data-wf-style-failure]')?.textContent).toContain('Your saved drafts are unchanged')
    expect(document.querySelector('button')?.textContent).toBe('Reload workspace')
    expect(document.querySelector('button')?.closest('[aria-hidden="true"]')).toBeNull()
    expect(document.querySelector('[data-wf-shell-skeleton]')?.getAttribute('aria-busy')).toBe('false')
    expect(link.media).toBe('print')
    link.dispatchEvent(new Event('load'))
    expect(link.media).toBe('all')
    expect(document.querySelector('[data-wf-style-failure]')).toBeNull()
    expect(document.querySelector('[data-wf-workspace-skeleton]')?.getAttribute('aria-hidden')).toBe('true')
    expect(document.querySelector('[data-wf-shell-skeleton]')?.getAttribute('aria-busy')).toBe('true')
  })
  it('defers the real StyleX link and enables it once loaded without unsafe inline handlers', () => {
    const tags = deferStylexDevTags(original)!
    expect(tags[0]).toBe(original[0])
    expect(tags[1]?.attrs).toEqual({ rel: 'stylesheet', href: '/virtual:stylex.css', media: 'print', 'data-wf-stylex-bootstrap': '' })
    expect(tags[1]?.attrs).not.toHaveProperty('onload')
    const link = document.createElement('link')
    link.rel = 'stylesheet'; link.media = 'print'; link.dataset.wfStylexBootstrap = ''
    document.head.append(link)
    const bootstrap = tags[2]?.children
    expect(typeof bootstrap).toBe('string')
    if (typeof bootstrap === 'string') new Function('document', bootstrap)(document)
    expect(link.media).toBe('print')
    link.dispatchEvent(new Event('load'))
    expect(link.media).toBe('all')
    expect(main).toContain('window.__wfStylexReady')
  })
  it('retains readiness after the dev runtime disables the already-loaded link', async () => {
    const link = document.createElement('link')
    link.rel = 'stylesheet'; link.media = 'print'; link.dataset.wfStylexBootstrap = ''
    document.head.append(link)
    const bootstrap = deferStylexDevTags(original)?.[2]?.children
    if (typeof bootstrap !== 'string') throw new Error('Missing StyleX bootstrap')
    new Function('document', bootstrap)(document)
    const ready = window.__wfStylexReady
    expect(ready).toBeInstanceOf(Promise)
    link.dispatchEvent(new Event('load'))
    link.disabled = true
    expect(link.sheet).toBeNull()
    await expect(ready).resolves.toBeUndefined()
  })
  it('resolves when the runtime stylesheet replaces the link before its load event', async () => {
    const link = document.createElement('link')
    link.rel = 'stylesheet'; link.media = 'print'; link.dataset.wfStylexBootstrap = ''
    document.head.append(link)
    const bootstrap = deferStylexDevTags(original)?.[2]?.children
    if (typeof bootstrap !== 'string') throw new Error('Missing StyleX bootstrap')
    new Function('document', bootstrap)(document)
    const style = document.createElement('style')
    style.id = '__stylex_virtual__'
    style.textContent = 'body { color: red; }'
    document.head.append(style)
    link.disabled = true
    await expect(window.__wfStylexReady).resolves.toBeUndefined()
    expect(link.media).toBe('all')
  })
  it('enables several built stylesheets together only once every one has loaded', async () => {
    const links = ['/assets/a.css', '/assets/b.css'].map(href => {
      const link = document.createElement('link')
      link.rel = 'stylesheet'; link.href = href; link.media = 'print'; link.dataset.wfStylexBootstrap = ''
      document.head.append(link)
      return link
    })
    const bootstrap = deferStylexDevTags(original)?.[2]?.children
    if (typeof bootstrap !== 'string') throw new Error('Missing StyleX bootstrap')
    new Function('document', bootstrap)(document)
    let ready = false
    void window.__wfStylexReady?.then(() => { ready = true })
    links[0]!.dispatchEvent(new Event('load'))
    await Promise.resolve()
    expect(links.map(link => link.media)).toEqual(['print', 'print'])
    expect(ready).toBe(false)
    links[1]!.dispatchEvent(new Event('load'))
    await window.__wfStylexReady
    expect(links.map(link => link.media)).toEqual(['all', 'all'])
  })
  it('keeps the old stylesheet as a failing render-blocking control', () => {
    expect(original[1]?.attrs?.media).toBeUndefined()
    expect(original[1]?.attrs?.rel).toBe('stylesheet')
  })
  it('does not introduce a bootstrap when there are no development stylesheet tags', () => {
    expect(deferStylexDevTags(null)).toBeNull()
    expect(deferStylexDevTags([original[0]!])).toEqual([original[0]])
  })
})

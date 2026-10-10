// @vitest-environment jsdom
import * as React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { expect, it, vi } from 'vitest'
import { Markdown } from '../../../../packages/fractal-ui/src/assistant-ui/composition/Markdown.tsx'
vi.mock('@stylexjs/stylex', () => ({ create: (v: unknown) => v, defineVars: (v: unknown) => v, props: () => ({}) }))
const render = (text: string, streaming = false) => {
  const node = document.createElement('div')
  node.innerHTML = renderToStaticMarkup(<Markdown text={text} streaming={streaming} />)
  return node
}
it.each([false, true])('preserves soft paragraph lines while streaming=%s', streaming => {
  const node = render('a\nb\n\nc\n**d\ne**', streaming)
  expect(node.querySelectorAll('p')).toHaveLength(2)
  expect(node.querySelectorAll('p br')).toHaveLength(3)
  expect(node.querySelector('p')?.innerHTML).toBe('a<br>b')
})
it('does not change fenced code or list continuation breaks', () => {
  const node = render('```text\na\nb\n```\n\n- a\n  b')
  expect(node.querySelector('pre code')?.textContent).toBe('a\nb\n')
  expect(node.querySelectorAll('pre br, li br')).toHaveLength(0)
})

import React from 'react'
import { addons, types, useStorybookState } from 'storybook/manager-api'

function BookIdentity() {
  const { refId } = useStorybookState()
  const book = refId === 'fractal-app' ? 'Fractal App · ref fractal-app' : 'Fractal Kit · local'
  return <span role="status" data-testid="fractal-book-identity" title={`Landing revision: ${process.env.STORYBOOK_FRACTAL_REVISION}`} style={{ padding: '0 12px', whiteSpace: 'nowrap', fontSize: 12 }}>{book} · landing {process.env.STORYBOOK_FRACTAL_REVISION}</span>
}

addons.register('fractal/book-identity', () => {
  addons.add('fractal/book-identity/toolbar', { type: types.TOOL, title: 'Book identity', match: ({ viewMode }) => viewMode === 'story' || viewMode === 'docs', render: BookIdentity })
})

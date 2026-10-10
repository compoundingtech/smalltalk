import { describe, expect, it } from 'vitest'
import { addTabAtPath, findGroupPath, group, nodeAtPath, paneKey, paneKind, paneTitle, parsePaneKey, replaceAtPath, split, splitGroupAtPath, walkSplits } from '@smalltalk/fractal-ui/assistant-ui/workbench-model'

describe('released pure workbench model (not mounted in the live workbench)', () => {
  it('preserves pane view/form identity and identifies unknown resources without fabricating content', () => {
    const pane = { uri: 'agent:agent/local-test', form: 'thread', view: 'local-view' }
    expect(parsePaneKey(paneKey(pane))).toEqual(pane)
    expect(paneKey(pane)).not.toBe(paneKey({ ...pane, view: 'other-view' }))
    expect(paneKind(pane)).toBe('thread')
    expect(paneKind({ uri: 'unknown:local-test' })).toBe('placeholder')
    expect(paneTitle(pane)).toBe('local-test')
  })
  it('changes only the selected nested group and retains sibling object identity and pane order', () => {
    const first = group([{ uri: 'agent:a' }])
    const second = group([{ uri: 'diff:captured/a' }])
    const layout = split('right', first, second, 0.4)
    const changed = addTabAtPath(layout, '0.1', { uri: 'terminal:local-test' })
    expect(nodeAtPath(changed, '0.0')).toBe(first)
    expect(nodeAtPath(changed, '0.1')).toEqual(group([{ uri: 'diff:captured/a' }, { uri: 'terminal:local-test' }]))
    expect(second.tabs).toEqual([{ uri: 'diff:captured/a' }])
    expect(findGroupPath(changed, 'terminal:local-test')).toBe('0.1')
    expect(addTabAtPath(changed, '0.1', { uri: 'terminal:local-test' })).toBe(changed)
  })
  it('rejects invalid paths without changing the tree and keeps controlled binary split axes/ratios', () => {
    const initial = group([{ uri: 'agent:a' }])
    const layout = splitGroupAtPath(initial, '0', 'below', { uri: 'diff:captured/a' })
    expect(walkSplits(layout)).toEqual([{ path: '0', node: { kind: 'split', split: 'below', ratio: 0.5, children: [initial, group([{ uri: 'diff:captured/a' }])] } }])
    for (const invalid of ['1', '0.2', '0.0.1', '0.nope']) {
      expect(replaceAtPath(layout, invalid, group([]))).toBe(layout)
      expect(addTabAtPath(layout, invalid, { uri: 'unknown:local-test' })).toBe(layout)
      expect(nodeAtPath(layout, invalid)).toBeNull()
    }
  })
})

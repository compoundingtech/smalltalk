import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Markdown, type MarkdownImageResolver } from './composition/Markdown'
import { accentTokens, tokens } from './embrace-tokens.stylex'
import type { ToolDiff } from './embrace-tool-preview'

export const EmbraceDiffPreview = ({ diffs }: { readonly diffs: readonly ToolDiff[] }) => (
  <div {...stylex.props(styles.diffList)}>
    {diffs.map((diff, index) => (
      <figure key={`${diff.path}:${index}`} {...stylex.props(styles.diff)}>
        <figcaption {...stylex.props(styles.caption)}>
          <span>{diff.path || 'Edited excerpt'}{diff.excerpt ? ' · excerpt' : ''}</span>
          <span {...stylex.props(styles.countAdded)}>+{diff.added} added</span>
          <span {...stylex.props(styles.countRemoved)}>−{diff.removed} removed</span>
        </figcaption>
        <div {...stylex.props(styles.diffScroll)} tabIndex={0} role="region" aria-label={`Changes to ${diff.path || 'excerpt'}`}>
          <table {...stylex.props(styles.diffTable)}>
            <thead {...stylex.props(styles.visuallyHidden)}>
              <tr><th>Old line</th><th>New line</th><th>Change</th><th>Source</th></tr>
            </thead>
            <tbody>
              {diff.lines.map((line, lineIndex) => (
                <tr key={lineIndex} {...stylex.props(line.kind === 'added' && styles.added, line.kind === 'removed' && styles.removed, line.kind === 'hunk' && styles.hunk)}>
                  <td {...stylex.props(styles.lineNumber)}>{line.oldNumber ?? ''}</td>
                  <td {...stylex.props(styles.lineNumber)}>{line.newNumber ?? ''}</td>
                  <td {...stylex.props(styles.sign)}>
                    <span aria-hidden="true">{line.kind === 'added' ? '+' : line.kind === 'removed' ? '−' : ' '}</span>
                    <span {...stylex.props(styles.visuallyHidden)}>{line.kind}</span>
                  </td>
                  <td {...stylex.props(styles.source)}>{line.text || ' '}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </figure>
    ))}
  </div>
)


/** Shared consent-gated Markdown rendering for tool output. */
export const EmbraceMarkdownPreview = ({ markdown, resolveImage }: { readonly markdown: string; readonly resolveImage?: MarkdownImageResolver }) => (
  <Markdown text={markdown} resolveImage={resolveImage} />
)

const styles = stylex.create({
  diffList: { display: 'flex', flexDirection: 'column', gap: 10, minWidth: 0 },
  diff: { margin: 0, borderWidth: 1, borderStyle: 'solid', borderColor: tokens.line, borderRadius: 6, overflow: 'hidden' },
  caption: { display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', gap: 10, padding: 8, fontSize: 11, color: tokens.ink, backgroundColor: tokens.recess, overflowWrap: 'anywhere' },
  countAdded: { color: tokens.good },
  countRemoved: { color: tokens.danger },
  diffScroll: { overflowX: 'auto', outlineColor: { default: accentTokens.accent, ':focus-visible': accentTokens.accent }, outlineOffset: -2 },
  diffTable: { borderCollapse: 'collapse', width: '100%', fontFamily: 'monospace', fontSize: 11, lineHeight: 1.6, color: tokens.ink },
  lineNumber: { width: 36, minWidth: 36, paddingInline: 5, textAlign: 'right', color: tokens.muted, userSelect: 'none' },
  sign: { width: 16, textAlign: 'center', userSelect: 'none' },
  source: { whiteSpace: 'pre', paddingInlineEnd: 12 },
  added: { backgroundColor: tokens.added },
  removed: { backgroundColor: tokens.removed },
  hunk: { backgroundColor: tokens.recess, color: tokens.muted },
  visuallyHidden: { position: 'absolute', width: 1, height: 1, padding: 0, margin: -1, overflow: 'hidden', clipPath: 'inset(50%)', whiteSpace: 'nowrap', borderWidth: 0 },
})

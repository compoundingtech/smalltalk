import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { Components } from 'react-markdown'
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

// Each generated Markdown element consumes semantic StyleX tokens; no global prose CSS.
const markdownComponents: Components = {
  h1: ({ children }) => <h1 {...stylex.props(styles.heading, styles.h1)}>{children}</h1>,
  h2: ({ children }) => <h2 {...stylex.props(styles.heading, styles.h2)}>{children}</h2>,
  h3: ({ children }) => <h3 {...stylex.props(styles.heading, styles.h3)}>{children}</h3>,
  h4: ({ children }) => <h4 {...stylex.props(styles.heading, styles.h3)}>{children}</h4>,
  h5: ({ children }) => <h5 {...stylex.props(styles.heading, styles.h3)}>{children}</h5>,
  h6: ({ children }) => <h6 {...stylex.props(styles.heading, styles.h3)}>{children}</h6>,
  p: ({ children }) => <p {...stylex.props(styles.paragraph)}>{children}</p>,
  a: ({ children, href, title }) => <a href={href} title={title} rel="noreferrer" {...stylex.props(styles.link)}>{children}</a>,
  pre: ({ children }) => <pre {...stylex.props(styles.pre)}>{children}</pre>,
  code: ({ children }) => <code {...stylex.props(styles.code)}>{children}</code>,
  blockquote: ({ children }) => <blockquote {...stylex.props(styles.quote)}>{children}</blockquote>,
  ul: ({ children }) => <ul {...stylex.props(styles.list)}>{children}</ul>,
  ol: ({ children, start }) => <ol start={start} {...stylex.props(styles.list)}>{children}</ol>,
  table: ({ children }) => <div {...stylex.props(styles.diffScroll)}><table {...stylex.props(styles.markdownTable)}>{children}</table></div>,
  th: ({ children, style }) => <th {...stylex.props(styles.tableCell, styles.tableHeading)} style={style}>{children}</th>,
  td: ({ children, style }) => <td {...stylex.props(styles.tableCell)} style={style}>{children}</td>,
  hr: () => <hr {...stylex.props(styles.rule)} />,
}

/** Safe Markdown AST rendering, including GFM lists, tables and fenced code. */
export const EmbraceMarkdownPreview = ({ markdown }: { readonly markdown: string }) => (
  <div {...stylex.props(styles.markdown)}>
    <Markdown remarkPlugins={[remarkGfm]} components={markdownComponents}>{markdown}</Markdown>
  </div>
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
  markdown: { color: tokens.ink, fontSize: 13, lineHeight: 1.65, overflowWrap: 'anywhere', minWidth: 0 },
  heading: { marginBlockStart: 16, marginBlockEnd: 6, lineHeight: 1.3, fontWeight: 600 },
  h1: { fontSize: 20 },
  h2: { fontSize: 17 },
  h3: { fontSize: 14 },
  paragraph: { marginBlock: 8 },
  link: { color: accentTokens.accent, textDecoration: 'underline', outlineOffset: 3 },
  pre: { overflowX: 'auto', padding: 10, borderRadius: 5, backgroundColor: tokens.recess, marginBlock: 10 },
  code: { fontFamily: 'monospace', fontSize: '0.9em', backgroundColor: tokens.recess, borderRadius: 3 },
  quote: { marginInline: 0, paddingInlineStart: 12, borderInlineStartWidth: 3, borderInlineStartStyle: 'solid', borderInlineStartColor: tokens.line, color: tokens.muted },
  list: { paddingInlineStart: 24, marginBlock: 8 },
  markdownTable: { borderCollapse: 'collapse', marginBlock: 10, width: '100%' },
  tableCell: { padding: 6, borderWidth: 1, borderStyle: 'solid', borderColor: tokens.line },
  tableHeading: { backgroundColor: tokens.recess, fontWeight: 600 },
  rule: { borderWidth: 0, borderBlockStartWidth: 1, borderBlockStartStyle: 'solid', borderBlockStartColor: tokens.line, marginBlock: 16 },
})

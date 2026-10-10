import type { ReactNode } from 'react'
import { extensionViews } from './build.ts'
import type { DiffProps } from './contract.ts'

/** Public source views preserve the exact evidence when no rich block is compiled in. */
const SourceDiff = ({ id, source, caption }: DiffProps): ReactNode => (
  <figure id={id}>
    {caption === undefined ? null : <figcaption>{caption}</figcaption>}
    {source._tag === 'patch' ? <pre>{source.patch}</pre> : (
      <><figcaption>{source.path}</figcaption><section aria-label="Before"><pre>{source.oldContents}</pre></section>
      <section aria-label="After"><pre>{source.newContents}</pre></section></>
    )}
  </figure>
)
const SourceMarkdown = ({ source }: { readonly source: string }): ReactNode => <pre>{source}</pre>
export const Diff = extensionViews.Diff ?? SourceDiff
export const Markdown = extensionViews.Markdown ?? SourceMarkdown

import { Schema } from 'effect'
import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'

import { representations, type RendererManifest } from './contract.ts'
import { SubjectEnvelope } from './envelope.ts'
import { defineRenderer, type RenderProps } from './react.tsx'
import { makeRegistry, type RegisteredRenderer } from './registry.ts'

const generic: RegisteredRenderer<string> = {
  manifest: {
    id: 'facts',
    match: { level: 'generic' },
    representations,
    needs: { live: 'none', actions: [] },
  },
  bindings: {
    inline: () => ({ ok: true, view: 'facts' }),
    row: () => ({ ok: true, view: 'facts' }),
    card: () => ({ ok: true, view: 'facts' }),
    detail: () => ({ ok: true, view: 'facts' }),
  },
}
const genericNative = {
  manifest: {
    id: 'native-facts',
    family: '*',
    presentations: ['detail', 'overview', 'resources'] as const,
    reason: 'Unknown native subjects remain readable.',
  },
  view: 'native-facts',
}
const genericObservation = {
  manifest: {
    id: 'graph-facts',
    kind: '*',
    representations,
    reason: 'Unknown observations retain their facts.',
  },
  view: 'graph-facts',
}

describe('subject registry boundaries', () => {
  it('selects by family and presentation without requiring an observed envelope', () => {
    const registry = makeRegistry({
      generic,
      renderers: [],
      genericNative,
      nativeRenderers: [
        {
          manifest: {
            id: 'conversation',
            family: 'agent',
            presentations: ['detail'],
            reason: 'Conversation streaming and composer lifecycle.',
          },
          view: 'conversation',
        },
      ],
    })
    expect(registry.subjectPlan({ ref: 'agent/compiler', presentation: 'detail' })).toMatchObject({
      _tag: 'Subject',
      rendererId: 'conversation',
      address: { ref: 'agent/compiler', presentation: 'detail' },
    })
    expect(registry.subjectPlan({ ref: 'agent/compiler', presentation: 'overview' })).toMatchObject(
      { _tag: 'Subject', rendererId: 'native-facts' },
    )
    expect(registry.subjectPlan({ ref: 'future/subject', presentation: 'detail' })).toMatchObject({
      _tag: 'Subject',
      rendererId: 'native-facts',
    })
    expect(registry.subjectPlan({ ref: 'not-a-ref', presentation: 'detail' })._tag).toBe(
      'MalformedSubject',
    )
    expect(registry.subjectPlan({ ref: 'agent/compiler', presentation: 'arbitrary' })._tag).toBe(
      'MalformedSubject',
    )
  })
  it('keeps envelope-only fixture registries honestly unavailable for native subjects', () => {
    expect(
      makeRegistry({ generic, renderers: [] }).subjectPlan({
        ref: 'agent/compiler',
        presentation: 'detail',
      })._tag,
    ).toBe('UnavailableSubjectRenderer')
  })
  it('selects observed semantic bodies only for the registered kind and representation', () => {
    const registry = makeRegistry({
      generic,
      renderers: [],
      genericObservation,
      observationRenderers: [
        {
          manifest: {
            id: 'pull-request',
            kind: 'vcs.pull-request',
            representations: ['card'],
            reason: 'Review summary body.',
          },
          view: 'pull-request',
        },
      ],
    })
    expect(
      registry.observationPlan({ kind: 'vcs.pull-request', representation: 'card' }),
    ).toMatchObject({ _tag: 'Observation', rendererId: 'pull-request' })
    expect(
      registry.observationPlan({ kind: 'vcs.pull-request', representation: 'row' }),
    ).toMatchObject({ _tag: 'Observation', rendererId: 'graph-facts' })
    expect(registry.observationPlan({ kind: 'future.kind', representation: 'card' })).toMatchObject(
      { _tag: 'Observation', rendererId: 'graph-facts' },
    )
    expect(registry.observationPlan({ kind: '', representation: 'card' })._tag).toBe(
      'MalformedObservation',
    )
  })
  it('rejects incomplete fallback claims and overlapping native presentation claims', () => {
    expect(() =>
      makeRegistry({
        generic: { ...generic, manifest: { ...generic.manifest, representations: ['detail'] } },
        renderers: [],
      }),
    ).toThrow()
    expect(() =>
      makeRegistry({
        generic,
        renderers: [],
        genericNative: {
          ...genericNative,
          manifest: { ...genericNative.manifest, presentations: ['detail'] },
        },
      }),
    ).toThrow()
    expect(() =>
      makeRegistry({
        generic,
        renderers: [],
        genericNative,
        nativeRenderers: [1, 2].map((id) => ({
          manifest: {
            id: `agent-${id}`,
            family: 'agent',
            presentations: ['detail'],
            reason: 'Native agent body.',
          },
          view: `agent-${id}`,
        })),
      }),
    ).toThrow()
  })
  it.each([
    [undefined, 'vcs.*'],
    ['vcs.*', 'vcs.pull-request'],
    ['vcs.*', 'vcs.pull*'],
    ['*', 'vcs.pull-request'],
  ])('rejects intersecting family kind claims %s / %s', (a, b) => {
    const renderer = (id: string, kind: string | undefined): RegisteredRenderer<string> => ({
      ...generic,
      manifest: {
        id,
        match: { level: 'family', family: 'resource', ...(kind === undefined ? {} : { kind }) },
        representations: ['card'],
        needs: { live: 'none', actions: [] },
      } satisfies RendererManifest,
    })
    expect(() =>
      makeRegistry({ generic, renderers: [renderer('first', a), renderer('second', b)] }),
    ).toThrow()
  })
})

describe('closed envelope renderer bindings', () => {
  const fallbackData = Schema.Struct({ count: Schema.optionalKey(Schema.FiniteFromString) })
  const exactData = Schema.Struct({ count: Schema.FiniteFromString, title: Schema.String })
  const Fallback = ({ data, issue }: RenderProps<typeof fallbackData.Type>) =>
    createElement(
      'p',
      null,
      `fallback:${data.count?.toFixed(1) ?? 'none'}:${issue?.rejectedBy ?? 'none'}`,
    )
  const Exact = ({ data }: RenderProps<typeof exactData.Type>) =>
    createElement('p', null, `${data.title.toUpperCase()}:${data.count.toFixed(1)}`)
  const registry = makeRegistry({
    generic: defineRenderer({
      id: 'fallback',
      match: { level: 'generic' },
      needs: { live: 'none', actions: [] },
      data: fallbackData,
      views: { inline: Fallback, row: Fallback, card: Fallback, detail: Fallback },
    }),
    renderers: [
      defineRenderer({
        id: 'exact',
        match: { level: 'exact', schema: 'st3.resource:counter', majors: { min: 1, max: 1 } },
        needs: { live: 'none', actions: [] },
        data: exactData,
        views: { detail: Exact },
      }),
    ],
  })
  const envelope = SubjectEnvelope.make({
    ref: 'resource/counter',
    family: 'resource',
    schema: 'st3.resource:counter@1',
    revision: 'fixture',
    observed_at: '2026-10-01T00:00:00Z',
    provenance: { source: 'projection', snapshot_id: 'fixture', host_id: 'host/fixture' },
    data: { count: '4' },
    actions: [],
    live: null,
  })
  it('renders schema-transformed data through its own typed view', () => {
    const plan = registry.plan({
      envelope: { ...envelope, data: { count: '4', title: 'counter' } },
      representation: 'detail',
      density: 'regular',
      host: { live: false, actions: false },
    })
    expect(plan._tag).toBe('Render')
    expect(renderToStaticMarkup(plan.view)).toBe('<p>COUNTER:4.0</p>')
  })
  it('decodes rejected data through the fallback schema before rendering its diagnostic', () => {
    const plan = registry.plan({
      envelope,
      representation: 'detail',
      density: 'regular',
      host: { live: false, actions: false },
    })
    expect(plan._tag).toBe('Invalid')
    expect(renderToStaticMarkup(plan.view)).toBe('<p>fallback:4.0:exact</p>')
  })
})

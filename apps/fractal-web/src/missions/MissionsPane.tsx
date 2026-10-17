import { useAtomValue } from '@effect/atom-react'
import { SystemNotice } from '@smalltalk/fractal-ui/assistant-ui/shell'
import type * as St3 from '@smalltalk/st3-client/schema'
import * as stylex from '@stylexjs/stylex'
import { DateTime } from 'effect'
import * as React from 'react'
import * as Aria from 'react-aria-components'

import { ago } from '../../../../clients/typescript/st3-views/time.ts'
import { colorVars as c, spaceVars as s, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { useDataSource, useNow } from '../data/react.tsx'
import { hiddenByDefault, missionWord, presentMission, sortMissions, type MissionPresentation, type PresentedStep, type StepWord } from './presentation.ts'

// The Missions screen: every mission with its word and who must act, in the contract's order,
// as stui's Missions tab lists them. A row opens to its steps. St's own missions, ones never
// started and ones settled before today are folded away until "Show all".

const STEP_GLYPH: Record<StepWord, string> = { needs_you: '◆', done: '✓', working: '⠋', ready: '◇', waiting: '◐', cancelled: '⊘', failed: '✕', pending: '○' }
const STEP_NAME: Record<StepWord, string> = { needs_you: 'needs you', done: 'done', working: 'working', ready: 'ready', waiting: 'waiting', cancelled: 'cancelled', failed: 'failed', pending: 'pending' }

type Row = { readonly mission: St3.Mission; readonly presentation: MissionPresentation }

const startOfDay = (now: number) => { const day = new Date(now); day.setHours(0, 0, 0, 0); return day.getTime() }

export const MissionsPane = ({ onOpenAgent }: { readonly onOpenAgent?: (ref: string) => void }): React.ReactNode => {
  const source = useDataSource()
  const missions = useAtomValue(source.missions)
  const agents = useAtomValue(source.agents)
  const attention = useAtomValue(source.attention)
  const now = useNow()
  const [all, setAll] = React.useState(false)
  const [open, setOpen] = React.useState<string>()
  const rows = React.useMemo((): readonly Row[] => {
    if (missions._tag !== 'Observed') return []
    const agentRows = agents._tag === 'Observed' ? agents.value : []
    const openAttention = attention._tag === 'Observed' ? attention.value.filter((item) => item.state === 'open') : []
    return sortMissions(missions.value.map((mission) => ({ mission, presentation: presentMission({ mission, agents: agentRows, attention: openAttention }) })))
  }, [missions, agents, attention])
  const today = startOfDay(now)
  const shown = all ? rows : rows.filter((row) => !hiddenByDefault(row.presentation, today))
  const hidden = rows.length - shown.length
  return (
    <section aria-label="Missions" data-testid="missions-pane" {...stylex.props(styles.pane)}>
      <header {...stylex.props(styles.header)}>
        <strong>Missions</strong>
        {missions._tag === 'Observed' && missions.freshness === 'stale' ? <span {...stylex.props(styles.meta)}>last observed · live updates pending</span> : null}
        {hidden > 0 || all ? (
          <Aria.Button onPress={() => setAll(!all)} {...stylex.props(styles.button)}>
            {all ? 'Hide settled and st’s own' : `Show all (${hidden} more)`}
          </Aria.Button>
        ) : null}
      </header>
      <div {...stylex.props(styles.body)}>
        {missions._tag === 'Waiting' ? <SystemNotice kind="loading" title="Loading missions" />
          : missions._tag === 'Unavailable' ? <SystemNotice
            kind="error"
            connection={missions.reason !== 'ungranted' && missions.reason !== 'unsupported'}
            title={missions.reason === 'ungranted' ? 'No access to missions' : missions.reason === 'unsupported' ? 'Missions not supported' : 'Missions could not load'}
            {...(missions.reason === 'ungranted' ? { detail: 'Read access has not been granted' } : missions.reason === 'unsupported' ? { detail: 'This connection does not offer them' } : {})}
          />
            : shown.length === 0 ? <SystemNotice kind="empty" title={rows.length === 0 ? 'No missions' : 'Nothing running today'} />
              : (
                <ul {...stylex.props(styles.list)}>
                  {shown.map(({ mission, presentation }) => {
                    const word = missionWord(presentation.word)
                    const expanded = open === mission.id
                    return (
                      <li key={mission.id} {...stylex.props(styles.item)}>
                        <Aria.Button aria-expanded={expanded} onPress={() => setOpen(expanded ? undefined : mission.id)} {...stylex.props(styles.row)}>
                          <span aria-hidden="true" {...stylex.props(styles.glyph, presentation.word === 'decision' && styles.person, ['stalled', 'failed'].includes(presentation.word) && styles.fault)}>{word.glyph}</span>
                          <span {...stylex.props(styles.word)} title={word.explain}>{word.name}</span>
                          <span {...stylex.props(styles.title)}>{mission.title}</span>
                          <span {...stylex.props(styles.meta)}>
                            {presentation.who.kind === 'nobody' ? '' : `${presentation.who.label} · `}
                            {presentation.progress.total > 0 ? `${presentation.progress.done}/${presentation.progress.total} · ` : ''}
                            {ago(DateTime.formatIso(mission.updated_at), now)}
                          </span>
                        </Aria.Button>
                        {presentation.last_progress && !expanded ? <div {...stylex.props(styles.progress)}>{presentation.last_progress}</div> : null}
                        {expanded ? <MissionDetail mission={mission} presentation={presentation} onOpenAgent={onOpenAgent} /> : null}
                      </li>
                    )
                  })}
                </ul>
              )}
      </div>
    </section>
  )
}

const MissionDetail = ({ mission, presentation, onOpenAgent }: { readonly mission: St3.Mission; readonly presentation: MissionPresentation; readonly onOpenAgent?: (ref: string) => void }): React.ReactNode => {
  const word = missionWord(presentation.word)
  return (
    <div {...stylex.props(styles.detail)} aria-label={`${mission.title} steps`}>
      <div {...stylex.props(styles.meta)}>{word.explain} · {mission.id}</div>
      {presentation.stuck?.agent ? (
        <div {...stylex.props(styles.stuck)}>
          Stuck at {presentation.stuck.step.split('/').pop()}{presentation.stuck.attempts > 1 ? ` after ${presentation.stuck.attempts} attempts` : ''}; its seat {presentation.stuck.agent.label} is {presentation.stuck.agent.state}.
        </div>
      ) : null}
      {presentation.last_progress ? <div {...stylex.props(styles.progress)}>Last progress: {presentation.last_progress}</div> : null}
      {presentation.steps.length === 0 ? <p {...stylex.props(styles.notice)}>st did not send this mission's steps.</p> : (
        <ol {...stylex.props(styles.steps)}>
          {presentation.steps.map((step) => <StepRow key={step.id} step={step} onOpenAgent={onOpenAgent} />)}
        </ol>
      )}
    </div>
  )
}

const StepRow = ({ step, onOpenAgent }: { readonly step: PresentedStep; readonly onOpenAgent?: (ref: string) => void }): React.ReactNode => (
  <li {...stylex.props(styles.step)}>
    <span aria-hidden="true" {...stylex.props(styles.glyph, step.word === 'needs_you' && styles.person, step.word === 'failed' && styles.fault)}>{STEP_GLYPH[step.word]}</span>
    <span {...stylex.props(styles.stepWord)}>{STEP_NAME[step.word]}</span>
    <span {...stylex.props(styles.title)}>{step.title}</span>
    {step.owner.kind === 'agent' && step.owner.agent !== undefined && onOpenAgent !== undefined
      ? <Aria.Button onPress={() => onOpenAgent(step.owner.agent!)} {...stylex.props(styles.link)}>{step.owner.label}</Aria.Button>
      : <span {...stylex.props(styles.meta)}>{step.owner.label}</span>}
    {step.attempts > 1 ? <span {...stylex.props(styles.meta)}>attempt {step.attempts}</span> : null}
    {step.note ? <span {...stylex.props(styles.note)}>{step.note}</span> : null}
  </li>
)

const styles = stylex.create({
  pane: { position: 'absolute', inset: 0, zIndex: 2, display: 'flex', flexDirection: 'column', minHeight: 0, backgroundColor: c.canvas, color: c.fg, fontFamily: t.fontSans, fontSize: t.uiSize },
  header: { display: 'flex', alignItems: 'baseline', gap: s.md, paddingInline: s.lg, paddingBlock: s.md, borderBottomWidth: 1, borderBottomStyle: 'solid', borderBottomColor: c.border, flexShrink: 0 },
  body: { flexGrow: 1, minHeight: 0, overflowY: 'auto', padding: s.md },
  button: { marginInlineStart: 'auto', padding: '4px 8px', borderWidth: 0, borderRadius: 6, backgroundColor: 'transparent', color: c.fgMuted, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: c.rowHover } },
  list: { margin: 0, padding: 0, listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 2 },
  item: { borderRadius: 6 },
  row: { width: '100%', display: 'grid', gridTemplateColumns: '1.25rem 6.5rem minmax(0, 1fr) auto', alignItems: 'baseline', columnGap: s.md, padding: s.md, borderWidth: 0, borderRadius: 6, backgroundColor: 'transparent', color: c.fg, textAlign: 'start', fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: c.rowHover }, ':focus-visible': { outline: `2px solid ${c.primary}` } },
  glyph: { color: c.fgMuted, textAlign: 'center' },
  person: { color: c.attention },
  fault: { color: c.dangerFg },
  word: { color: c.fgMuted },
  title: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  meta: { color: c.fgMuted, fontSize: t.denseSize, whiteSpace: 'nowrap' },
  progress: { paddingInlineStart: 'calc(1.25rem + 6.5rem + 24px)', color: c.fgMuted, fontSize: t.denseSize, overflowWrap: 'anywhere' },
  detail: { display: 'flex', flexDirection: 'column', gap: s.sm, paddingBlock: s.sm, paddingInlineStart: 'calc(1.25rem + 16px)', paddingInlineEnd: s.md },
  stuck: { color: c.dangerFg, fontSize: t.metaSize },
  steps: { margin: 0, padding: 0, listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 2 },
  step: { display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', columnGap: s.md, fontSize: t.metaSize },
  stepWord: { color: c.fgMuted, minWidth: '5.5rem' },
  link: { padding: 0, borderWidth: 0, backgroundColor: 'transparent', color: c.primary, fontSize: t.denseSize, cursor: 'pointer', textDecoration: 'underline' },
  note: { flexBasis: '100%', paddingInlineStart: 'calc(1.25rem + 5.5rem + 24px)', color: c.fgMuted, fontSize: t.denseSize },
  notice: { color: c.fgMuted, fontSize: t.metaSize, padding: s.md },
})

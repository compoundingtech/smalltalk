import * as stylex from '@stylexjs/stylex'
import { Button, Tooltip, TooltipTrigger } from 'react-aria-components'
import { accentVars as accent, borderVars as border, geometryVars as geometry, radiusVars as radius, spaceVars as space, surfaceVars as surface, textVars as text, typeVars as type } from '../composition-tokens.stylex'

/** An inline missing value beside its painted label; source provenance is available to keyboard and pointer users. */
export function MissingDetailsValue({ label, reason }: { readonly label: string; readonly reason: string }) {
  return <TooltipTrigger delay={0}><Button data-details-missing-value aria-label={`${label}: unavailable`} {...stylex.props(styles.trigger)}>—</Button><Tooltip {...stylex.props(styles.tooltip)}>{reason}</Tooltip></TooltipTrigger>
}

export type DetailsAvailability = 'Unavailable' | 'Partial'

/** The painted state word is the provenance trigger; its accessible name keeps the subject it qualifies. */
export function DetailsAvailabilityState({ subject, state = 'Unavailable', reason }: { readonly subject: string; readonly state?: DetailsAvailability; readonly reason: string }) {
  return <TooltipTrigger delay={0}><Button data-details-availability={state} aria-label={`${subject}: ${state.toLowerCase()}`} {...stylex.props(styles.trigger, styles.word)}>{state}</Button><Tooltip {...stylex.props(styles.tooltip)}>{reason}</Tooltip></TooltipTrigger>
}

/** Subjects read as one phrase: deduped, comma-joined, sentence case. Pending footprints paint the same text. */
export const unavailableSubject = (subjects: readonly string[]): string =>
  [...new Set(subjects)].map((name, index) => index === 0 ? name : name.charAt(0).toLowerCase() + name.slice(1)).join(', ')

/** One quiet line names what is missing and its state, never a bare state or em dash. */
export function DetailsUnavailable({ subjects, state = 'Unavailable', reason }: { readonly subjects: readonly string[]; readonly state?: DetailsAvailability; readonly reason: string }) {
  const subject = unavailableSubject(subjects)
  return <p data-details-unavailable data-details-unavailable-subject={subject} {...stylex.props(styles.line)}><span aria-hidden="true" {...stylex.props(styles.subject)}>{subject}</span><span aria-hidden="true">·</span><DetailsAvailabilityState subject={subject} state={state} reason={reason} /></p>
}

const styles = stylex.create({
  trigger: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', padding: space.zero, minWidth: geometry.icon, borderWidth: 0, borderRadius: radius.sm, backgroundColor: surface.transparent, color: text.fgMuted, fontFamily: type.fontSans, fontSize: type.metaSize, lineHeight: type.metaLeading, cursor: 'help', ':focus-visible': { outline: `2px solid ${accent.primary}`, outlineOffset: geometry.focusOffset } },
  word: { minWidth: 0, flexShrink: 0, whiteSpace: 'nowrap' },
  line: { display: 'flex', alignItems: 'baseline', flexWrap: 'wrap', columnGap: space.xs, margin: space.zero, color: text.fgMuted, fontSize: type.metaSize, lineHeight: type.metaLeading },
  subject: { minWidth: 0, overflowWrap: 'normal' },
  tooltip: { maxWidth: geometry.tooltipMax, padding: space.md, color: text.fg, backgroundColor: surface.raised, borderWidth: geometry.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: radius.sm, fontFamily: type.fontSans, fontSize: type.metaSize, lineHeight: type.metaLeading },
})

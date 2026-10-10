import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button } from 'react-aria-components'
import { Icon } from '../composition/Icons'
import { surfaceVars, textVars, borderVars, accentVars, radiusVars, spaceVars, typeVars, geometryVars } from '../composition-tokens.stylex'
import { observeAffordancePosition, returnAffordanceFocus } from './AffordancePosition'

/** Same floating action for measured and virtual lanes; it never consumes transcript layout space. */
export const FollowAffordance = React.memo(function FollowAffordance({ buttonRef, onPress, hidden = true }: {
  readonly buttonRef?: (button: HTMLButtonElement | null) => void
  readonly onPress: () => void
  readonly hidden?: boolean
}) {
  const attach = React.useCallback((button: HTMLButtonElement | null) => {
    buttonRef?.(button)
    if (button === null) return
    const disconnect = observeAffordancePosition(button)
    return () => { returnAffordanceFocus(button); disconnect?.(); buttonRef?.(null) }
  }, [buttonRef])
  return <Button ref={attach} hidden={hidden} aria-label="Scroll to end" onPointerDown={event => event.preventDefault()} onPress={onPress} {...stylex.props(styles.button)}><span {...stylex.props(styles.content)}><Icon name="chevron-down" size={14} />Scroll to end</span></Button>
})
const styles = stylex.create({
  button: {
    position: 'absolute', zIndex: 1, bottom: `calc(${spaceVars.xl} + ${spaceVars.xs2} + var(--fractal-follow-composer-gap, 0px))`, left: '50%', transform: 'translateX(-50%)',
    height: geometryVars.controlSm, boxSizing: 'border-box', paddingBlock: 0, paddingInline: `calc(${spaceVars.sm} + ${spaceVars.hairline})`,
    borderRadius: radiusVars.full, borderWidth: geometryVars.hairline, borderStyle: 'solid', borderColor: borderVars.glassBorder,
    backgroundColor: surfaceVars.glassFill, backdropFilter: `blur(${geometryVars.blur})`, color: textVars.fg,
    boxShadow: `0 ${spaceVars.xs2} ${spaceVars.md} ${surfaceVars.scrim}`, fontFamily: typeVars.fontSans, fontSize: typeVars.metaSize, lineHeight: typeVars.metaLeading, fontWeight: typeVars.weightMedium, whiteSpace: 'nowrap', cursor: 'pointer',
    ':focus-visible': { outlineWidth: geometryVars.separatorHitSlop, outlineStyle: 'solid', outlineColor: accentVars.primary, outlineOffset: geometryVars.focusOffset },
  },
  content: { display: 'inline-flex', alignItems: 'center', gap: spaceVars.xs, verticalAlign: 'middle' },
})

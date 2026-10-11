// One editor group: a labelled frame that reports focus-within, holding a header (or `EditorTabs`)
// and an `EditorDropZone` body. The drop zone uses react-aria `useDrop`, so tabs dragged by mouse,
// touch or the keyboard/screen-reader drag mode can land: the outer quarter on each side splits
// in that direction, the centre moves the editor into this group.

import * as stylex from '@stylexjs/stylex'
import { useRef, useState, type ReactNode } from 'react'
import { useDrop } from 'react-aria'

import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

import { readTabDrop, type TabDragPayload } from './EditorTabs.tsx'

/** Drop region: the centre moves an editor, edges split in that direction. */
export type DropZone = 'center' | 'left' | 'right' | 'top' | 'bottom'

const styles = stylex.create({
  group: {
    display: 'flex',
    flexDirection: 'column',
    flexGrow: 1,
    minWidth: 0,
    minHeight: 0,
    backgroundColor: tokens['--ds-background-100'],
    position: 'relative',
  },
  card: {
    borderWidth: '1px',
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
    borderRadius: scale.radiusLg,
    overflow: 'hidden',
  },
  cardFocused: { borderColor: tokens['--ds-gray-alpha-600'] },
  // Keep content at full contrast; neutral chrome recedes while focus and attention stay distinct.
  ring: {
    borderWidth: '1px',
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
    borderRadius: scale.radiusDefault,
    overflow: 'hidden',
  },
  ringFocused: { borderColor: tokens['--ds-blue-700'] },
  ringAttention: { borderColor: tokens['--ds-amber-700'] },
  flatFocused: {
    '::before': {
      content: '""',
      position: 'absolute',
      top: 0,
      insetInline: 0,
      height: '1px',
      backgroundColor: tokens['--ds-gray-1000'],
      pointerEvents: 'none',
      zIndex: 2,
    },
  },
  attentionRing: {
    outlineWidth: '1px',
    outlineStyle: 'solid',
    outlineColor: tokens['--ds-amber-700'],
    outlineOffset: '-1px',
  },
  body: {
    position: 'relative',
    display: 'flex',
    flexDirection: 'column',
    flexGrow: 1,
    minHeight: 0,
    overflow: 'auto',
    outlineStyle: 'none',
  },
  overlay: {
    position: 'absolute',
    zIndex: 3,
    pointerEvents: 'none',
    backgroundColor: tokens['--ds-blue-200'],
    opacity: 0.7,
    borderWidth: '1px',
    borderStyle: 'solid',
    borderColor: tokens['--ds-blue-700'],
    borderRadius: scale.radiusDefault,
  },
  center: { inset: '4px' },
  left: { top: '4px', bottom: '4px', left: '4px', right: '50%' },
  right: { top: '4px', bottom: '4px', left: '50%', right: '4px' },
  top: { top: '4px', left: '4px', right: '4px', bottom: '50%' },
  bottom: { top: '50%', left: '4px', right: '4px', bottom: '4px' },
})

/** Visual treatment for group chrome: flat divider, card surface, or focus ring. */
export type GroupSurface = 'flat' | 'card' | 'ring'

/** Labelled group frame with focus and attention state. */
export interface EditorGroupProps {
  readonly label: string
  readonly isFocused: boolean
  readonly onFocusWithin: () => void
  /** `card` rounded surface (Z3), `ring` focus ring (Z4). */
  readonly surface?: GroupSurface
  /** Notification ring: only this pane's active surface needs attention; read does not resolve it. */
  readonly attention?: boolean
  readonly children: ReactNode
}

/** Group frame that reports focus from both keyboard and pointer interaction. */
export const EditorGroup = ({
  label,
  isFocused,
  onFocusWithin,
  surface = 'flat',
  attention = false,
  children,
}: EditorGroupProps) => (
  <section
    aria-label={label}
    data-focused={isFocused || undefined}
    data-attention={attention || undefined}
    onFocusCapture={onFocusWithin}
    onPointerDownCapture={onFocusWithin}
    {...stylex.props(
      styles.group,
      surface === 'flat' && isFocused && styles.flatFocused,
      attention && styles.attentionRing,
      surface === 'card' && styles.card,
      surface === 'card' && isFocused && styles.cardFocused,
      surface === 'ring' && styles.ring,
      surface === 'ring' && attention && styles.ringAttention,
      surface === 'ring' && isFocused && styles.ringFocused,
    )}
  >
    {children}
  </section>
)

const zoneOf = ({
  x,
  y,
  width,
  height,
}: {
  readonly x: number
  readonly y: number
  readonly width: number
  readonly height: number
}): DropZone => {
  const fx = x / width
  const fy = y / height
  const edge = Math.min(fx, 1 - fx, fy, 1 - fy)
  if (edge > 0.25) return 'center'
  if (edge === fx) return 'left'
  if (edge === 1 - fx) return 'right'
  return edge === fy ? 'top' : 'bottom'
}

/** Drop target for dragged tabs, optionally exposed as a labelled region. */
export interface EditorDropZoneProps {
  readonly dragType: string
  readonly onDropEditor: (drop: {
    readonly source: TabDragPayload
    readonly zone: DropZone
  }) => void
  /** Without tabs the body is a labelled region instead of a tabpanel. */
  readonly label?: string
  readonly children: ReactNode
}

/** Editor body that previews and reports the drop zone for dragged tabs. */
export const EditorDropZone = ({
  dragType,
  onDropEditor,
  label,
  children,
}: EditorDropZoneProps) => {
  const ref = useRef<HTMLDivElement>(null)
  const [zone, setZone] = useState<DropZone | null>(null)
  const zoneAt = ({ x, y }: { readonly x: number; readonly y: number }) => {
    const rect = ref.current?.getBoundingClientRect()
    return rect === undefined ? 'center' : zoneOf({ x, y, width: rect.width, height: rect.height })
  }
  const { dropProps } = useDrop({
    ref,
    getDropOperation: (types) => (types.has(dragType) ? 'move' : 'cancel'),
    onDropEnter: (event) => setZone(zoneAt(event)),
    onDropMove: (event) => setZone(zoneAt(event)),
    onDropExit: () => setZone(null),
    onDrop: async (event) => {
      const target = zoneAt(event)
      setZone(null)
      const payload = await readTabDrop({ items: event.items, dragType })
      if (payload !== null) onDropEditor({ source: payload, zone: target })
    },
  })
  return (
    <div
      ref={ref}
      {...(label === undefined ? {} : { role: 'region', 'aria-label': label })}
      {...dropProps}
      {...stylex.props(styles.body)}
    >
      {children}
      {zone === null ? null : (
        <div aria-hidden="true" {...stylex.props(styles.overlay, styles[zone])} />
      )}
    </div>
  )
}

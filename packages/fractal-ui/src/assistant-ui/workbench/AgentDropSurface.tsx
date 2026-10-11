import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { isTextDropItem, useDrop, type DropEvent } from 'react-aria'
import { AGENT_DRAG_MIME, type AgentPlacement } from './agent-drag'
import { WorkbenchPresentationContext } from './workbench-appearance'
import { accentVars as ac, surfaceVars as sf, textVars as tx, borderVars as bd, radiusVars as r, spaceVars as s, typeVars as t, geometryVars as g } from '../composition-tokens.stylex'

interface Props {
  readonly path: string
  readonly labelPrefix?: string
  readonly children: React.ReactNode
  readonly onOpen: (key: string, placement: AgentPlacement) => void
  readonly isDisabled: boolean
}
const labels: Record<AgentPlacement, string> = { center: 'Open as tab', right: 'Open in split right', below: 'Open in split below', left: 'Open in split left', above: 'Open in split above' }
const copyAgent = (types: { has: (type: string) => boolean }) => types.has(AGENT_DRAG_MIME) ? 'copy' as const : 'cancel' as const
async function readAgent(event: DropEvent, onOpen: Props['onOpen'], placement: AgentPlacement) {
  const item = event.items.find(item => isTextDropItem(item) && item.types.has(AGENT_DRAG_MIME))
  if (item && isTextDropItem(item)) onOpen(await item.getText(AGENT_DRAG_MIME), placement)
}

/** Portalled panes keep React ancestry outside the layout. Native drag events still bubble through
 * their physical drop container; bind Aria's handlers there instead of relying on React ancestry. */
function useNativeDropProps(props: React.DOMAttributes<HTMLElement>, ref: React.RefObject<HTMLElement | null>) {
  const { onDragEnter, onDragOver, onDragLeave, onDrop, ...focusProps } = props
  const handlers = React.useRef({ onDragEnter, onDragOver, onDragLeave, onDrop })
  handlers.current = { onDragEnter, onDragOver, onDragLeave, onDrop }
  React.useLayoutEffect(() => {
    const element = ref.current!
    const names = [['dragenter', 'onDragEnter'], ['dragover', 'onDragOver'], ['dragleave', 'onDragLeave'], ['drop', 'onDrop']] as const
    const listeners = names.map(([name, key]) => {
      const listener = (event: DragEvent) => handlers.current[key]?.(event as unknown as React.DragEvent<HTMLElement>)
      element.addEventListener(name, listener)
      return () => element.removeEventListener(name, listener)
    })
    return () => { for (const remove of listeners) remove() }
  }, [ref])
  return focusProps
}
const previewGeometry = (side: AgentPlacement): React.CSSProperties => ({
  '--drop-left': side === 'right' ? '50%' : '0%',
  '--drop-top': side === 'below' ? '50%' : '0%',
  '--drop-width': side === 'right' || side === 'left' ? '50%' : '100%',
  '--drop-height': side === 'below' || side === 'above' ? '50%' : '100%',
} as React.CSSProperties)

/** Aria owns the drop interaction; only this wrapper commits when drag focus changes.
 * Pointer movement paints a CSS-variable preview once per frame, never pane state. */
export const AgentDropSurface = React.memo(function AgentDropSurface({ path, children, onOpen, isDisabled, labelPrefix }: Props) {
  const presentation = React.useContext(WorkbenchPresentationContext)
  const { appearance } = presentation
  const previewPlacement = presentation.previewPath === path ? presentation.previewPlacement : undefined
  const variant = appearance?.dropZones ?? 'D1'
  const ref = React.useRef<HTMLDivElement>(null)
  const preview = React.useRef<HTMLDivElement>(null)
  const zones = React.useRef<HTMLDivElement>(null)
  const label = React.useRef<HTMLSpanElement>(null)
  const [chipTarget, setChipTarget] = React.useState(false)
  const pendingFrame = React.useRef<number | null>(null)
  const placement = React.useRef<AgentPlacement>(previewPlacement ?? 'center')
  const initialGeometry = React.useMemo(() => previewGeometry(previewPlacement ?? 'center'), [previewPlacement])
  const region = (x: number, y: number): AgentPlacement => {
    const rect = ref.current?.getBoundingClientRect()
    if (!rect || rect.width === 0 || rect.height === 0) return 'center'
    const right = x / rect.width, below = y / rect.height
    if (variant === 'D2') {
      if (below < 0.25) return 'above'
      if (below > 0.75) return 'below'
      return right < 0.25 ? 'left' : right > 0.75 ? 'right' : 'center'
    }
    return right >= 0.75 && right >= below ? 'right' : below >= 0.75 ? 'below' : 'center'
  }
  const paint = () => {
    pendingFrame.current = null
    const element = preview.current
    if (!element) return
    const side = placement.current
    for (const [name, value] of Object.entries(previewGeometry(side))) element.style.setProperty(name, String(value))
    element.dataset.placement = side
    if (label.current) label.current.textContent = labels[side]
    if (variant === 'D2') for (const target of zones.current?.querySelectorAll<HTMLElement>('[data-drop-zone]') ?? []) {
      const active = target.dataset.dropZone === side
      target.dataset.dropActive = String(active)
    }
  }
  const showPlacement = (side: AgentPlacement) => {
    if (placement.current === side && preview.current?.dataset.placement === side) return
    placement.current = side
    if (pendingFrame.current === null) pendingFrame.current = requestAnimationFrame(paint)
  }
  const move = ({ x, y }: { x: number; y: number }) => showPlacement(region(x, y))
  const clear = () => {
    if (pendingFrame.current !== null) cancelAnimationFrame(pendingFrame.current)
    pendingFrame.current = null
    if (variant === 'D2') {
      placement.current = previewPlacement ?? 'center'
      paint()
    }
  }
  const { dropProps, isDropTarget } = useDrop({
    ref, isDisabled, getDropOperation: copyAgent,
    onDropEnter: move, onDropMove: move, onDropExit: clear,
    onDrop: event => { clear(); setChipTarget(false); void readAgent(event, onOpen, region(event.x, event.y)) },
  })
  const focusProps = useNativeDropProps(dropProps, ref)
  const visible = isDropTarget || chipTarget || previewPlacement !== undefined
  return (
    <div {...focusProps} ref={ref} role="region" aria-label={`${labelPrefix === undefined ? '' : `${labelPrefix} · `}Drop agent into pane group ${path}`} tabIndex={0} data-drop-group={path} data-drop-zones={variant} {...stylex.props(styles.surface)}>
      {children}
      {visible && <div aria-hidden="true" data-testid="agent-drop-hit-layer" {...stylex.props(styles.hitLayer)} />}
      <div ref={preview} aria-hidden="true" data-testid="agent-drop-preview" data-placement={previewPlacement} style={initialGeometry} {...stylex.props(styles.preview, visible && variant !== 'D3' && styles.previewActive, variant === 'D2' && styles.gridPreview)}>
        {variant !== 'D3' && <span ref={label} {...stylex.props(styles.previewLabel, variant === 'D2' && styles.gridPreviewLabel)}>{labels[previewPlacement ?? 'center']}</span>}
      </div>
      {variant !== 'D1' && visible && <div ref={zones} aria-label="Drop positions" {...stylex.props(styles.zones, variant === 'D3' && styles.chips)}>
        {(variant === 'D2' ? ['left', 'right', 'above', 'below', 'center'] as const : ['center', 'right', 'below'] as const).map(side => <DropChip key={side} side={side} grid={variant === 'D2'} previewActive={(variant === 'D2' ? previewPlacement ?? 'center' : previewPlacement) === side} onActive={setChipTarget} onOpen={onOpen} onPreview={showPlacement} isDisabled={isDisabled} />)}
      </div>}
    </div>
  )
})
function DropChip({ side, grid, previewActive, onActive, onOpen, onPreview, isDisabled }: { side: AgentPlacement; grid: boolean; previewActive: boolean; onActive: (active: boolean) => void; onOpen: Props['onOpen']; onPreview: (side: AgentPlacement) => void; isDisabled: boolean }) {
  const ref = React.useRef<HTMLButtonElement>(null)
  const { dropProps, isDropTarget } = useDrop({ ref, isDisabled, getDropOperation: copyAgent, onDropEnter: () => { onActive(true); onPreview(side) }, onDropMove: () => onPreview(side), onDropExit: () => onActive(false), onDrop: event => { void readAgent(event, onOpen, side) } })
  const focusProps = useNativeDropProps(dropProps, ref)
  return <button {...focusProps} ref={ref} data-drop-chip={grid ? undefined : side} data-drop-zone={grid ? side : undefined} data-drop-active={grid ? previewActive : undefined} aria-label={labels[side]} {...stylex.props(!grid && styles.previewLabel, styles.dropChip, grid && styles.gridChip, grid && zoneStyles[side], !grid && (isDropTarget || previewActive) && styles.chipActive)}>{grid ? null : side === 'center' ? 'Tab' : side === 'right' ? 'Split right' : 'Split below'}</button>
}
const zoneStyles = stylex.create({
  left: { left: 0, top: '25%', width: '25%', height: '50%' },
  right: { right: 0, top: '25%', width: '25%', height: '50%' },
  above: { top: 0, left: 0, width: '100%', height: '25%' },
  below: { bottom: 0, left: 0, width: '100%', height: '25%' },
  center: { top: '25%', left: '25%', width: '50%', height: '50%' },
})
const styles = stylex.create({
  surface: { position: 'relative', display: 'flex', flexDirection: 'column', flexGrow: 1, minHeight: 0, minWidth: 0, overflow: 'hidden', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ac.primary, outlineOffset: `calc(-1 * ${g.focusOffset})` } },
  hitLayer: { position: 'absolute', inset: 0, zIndex: 1 },
  preview: { position: 'absolute', zIndex: 2, pointerEvents: 'none', opacity: 0, left: 'var(--drop-left)', top: 'var(--drop-top)', width: 'var(--drop-width)', height: 'var(--drop-height)', boxSizing: 'border-box', display: 'flex', alignItems: 'center', justifyContent: 'center', padding: s.lg, borderWidth: g.focusRing, borderStyle: 'solid', borderColor: ac.primary, borderRadius: r.md, backgroundColor: sf.rowActive, color: tx.fg, fontSize: t.metaSize, fontWeight: t.weightMedium },
  previewActive: { opacity: 1 },
  gridPreview: { alignItems: 'center', backgroundColor: `color-mix(in srgb, ${ac.primary} 22%, transparent)` },
  gridPreviewLabel: { pointerEvents: 'none' },
  previewLabel: { pointerEvents: 'auto', paddingInline: s.md, paddingBlock: s.xs, backgroundColor: sf.raised, color: tx.fg, borderRadius: r.control, borderWidth: g.hairline, borderStyle: 'solid', borderColor: bd.borderStrong, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  zones: { position: 'absolute', zIndex: 3, inset: 0, pointerEvents: 'none' },
  chips: { display: 'flex', alignItems: 'center', justifyContent: 'center', gap: s.xs, flexWrap: 'wrap', alignContent: 'center', padding: s.sm },
  dropChip: { minHeight: g.controlMd, cursor: 'copy', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ac.primary } },
  gridChip: { position: 'absolute', boxSizing: 'border-box', pointerEvents: 'auto', borderWidth: 0, borderColor: 'transparent', backgroundColor: 'transparent' },
  chipActive: { borderColor: ac.primary, backgroundColor: sf.rowActive },
})

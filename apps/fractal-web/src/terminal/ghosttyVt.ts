import { TerminalId, type TerminalLine, type TerminalModes, type TerminalRun, TerminalScreen } from '@smalltalk/st3-client/schema'
import * as Schema from 'effect/Schema'

class GhosttyError extends Schema.TaggedError<GhosttyError>()('GhosttyError', {
  message: Schema.String,
}) {}

/** One instantiated Ghostty lib-vt module (memory + exports + small alloc helpers). */
export interface GhosttyNative {
  readonly memory: WebAssembly.Memory
  readonly call: (name: string, ...args: (number | bigint)[]) => number
  readonly checked: (name: string, ...args: (number | bigint)[]) => void
}

const decoder = new TextDecoder('utf-8', { fatal: true })
const utf8 = new TextEncoder()
let compiled: Promise<WebAssembly.Module> | undefined

const instantiate = async (module: WebAssembly.Module): Promise<GhosttyNative> => {
  let memory: WebAssembly.Memory | undefined
  const instance = await WebAssembly.instantiate(module, {
    env: {
      // oxlint-disable-next-line overeng/named-args -- Native import ABI fixes positional arguments.
      log: (pointer: number, length: number) => {
        if (memory !== undefined) console.warn(decoder.decode(new Uint8Array(memory.buffer, pointer, length)))
      },
    },
  })
  if (!(instance.exports.memory instanceof WebAssembly.Memory))
    throw new GhosttyError({ message: 'Ghostty has no memory export' })
  memory = instance.exports.memory
  const call: GhosttyNative['call'] = (name, ...args) => {
    const fn = instance.exports[name]
    if (typeof fn !== 'function') throw new GhosttyError({ message: `Missing Ghostty export: ${name}` })
    const result: unknown = fn(...args)
    if (result === undefined) return 0
    if (typeof result !== 'number') throw new GhosttyError({ message: `Invalid Ghostty result: ${name}` })
    return result
  }
  return {
    memory,
    call,
    checked: (name, ...args) => {
      const result = call(name, ...args)
      if (result !== 0) throw new GhosttyError({ message: `Ghostty call failed (${name}: ${result})` })
    },
  }
}

/** Fetch and compile the committed asset once per page, instantiating fresh memory per call. */
export const loadGhosttyNative = async (): Promise<GhosttyNative> => {
  compiled ??= (async () => {
    const response = await fetch(new URL('./assets/ghostty-key-encoder.generated.wasm', import.meta.url))
    if (!response.ok) throw new GhosttyError({ message: `Ghostty unavailable: HTTP ${response.status}` })
    return WebAssembly.compile(await response.arrayBuffer())
  })()
  return instantiate(await compiled)
}

/** Instantiate the same native asset from supplied bytes, including in Node tests. */
export const instantiateGhosttyNative = async (bytes: BufferSource): Promise<GhosttyNative> =>
  instantiate(await WebAssembly.compile(bytes))

/** A native VT emulator with reference-stable projected viewport snapshots. */
export interface GhosttyTerminal {
  write(bytes: Uint8Array): void
  /** Full reset before a SCREEN replay. */
  reset(): void
  resize(size: { readonly cols: number; readonly rows: number }): void
  /** Negative deltas scroll up; bottom follows the active area. */
  scroll(delta: number | 'bottom'): void
  /** Whether the viewport follows the active area rather than history. */
  following(): boolean
  snapshot(meta: { readonly terminalId: string; readonly incarnation: string; readonly fallbackTitle: string }): TerminalScreen
  modes(): TerminalModes
  dispose(): void
}

type RunStyle = Omit<TerminalRun, 'text' | 'cells'>
const sameStyle = (a: RunStyle, b: RunStyle) =>
  a.fg === b.fg && a.bg === b.bg && a.bold === b.bold && a.dim === b.dim &&
  a.italic === b.italic && a.underline === b.underline && a.inverse === b.inverse &&
  a.strikethrough === b.strikethrough && a.link?.uri === b.link?.uri
const sameLine = (a: TerminalLine, b: TerminalLine) =>
  a.text === b.text && a.wrapped === b.wrapped && a.truncated === b.truncated &&
  a.runs.length === b.runs.length && a.runs.every((run, index) => {
    const other = b.runs[index]
    return other !== undefined && run.text === other.text && run.cells === other.cells && sameStyle(run, other)
  })

/** Create a terminal, retaining reusable native scratch buffers until disposal. */
export const makeGhosttyTerminal = (
  native: GhosttyNative,
  options: { readonly cols: number; readonly rows: number; readonly scrollbackBytes?: number },
): GhosttyTerminal => {
  const { call, checked, memory } = native
  const view = () => new DataView(memory.buffer)
  const allocations = new Map<number, number>()
  const alloc = (size: number) => {
    const pointer = call('ghostty_wasm_alloc_u8_array', size)
    if (pointer === 0) throw new GhosttyError({ message: 'Ghostty scratch allocation failed' })
    allocations.set(pointer, size)
    return pointer
  }
  const free = (pointer: number) => {
    const size = allocations.get(pointer)
    if (size !== undefined) {
      call('ghostty_wasm_free_u8_array', pointer, size)
      allocations.delete(pointer)
    }
  }
  let terminal = 0
  let render = 0
  let iterator = 0
  let cells = 0
  let disposed = false
  const dispose = () => {
    if (disposed) return
    disposed = true
    call('ghostty_render_state_row_cells_free', cells)
    call('ghostty_render_state_row_iterator_free', iterator)
    call('ghostty_render_state_free', render)
    call('ghostty_terminal_free', terminal)
    for (const pointer of allocations.keys()) free(pointer)
  }
  const assertLive = () => {
    if (disposed) throw new GhosttyError({ message: 'Ghostty terminal was disposed' })
  }
  const slot = alloc(32)
  const style = alloc(72)
  const buffer = alloc(12)
  const point = alloc(24)
  const ref = alloc(12)
  const behavior = alloc(24)
  const zero = alloc(4)
  view().setUint32(zero, 0, true)
  let scratchCapacity = 256
  let scratch = alloc(scratchCapacity)
  const grow = (required: number) => {
    if (required <= scratchCapacity) return
    free(scratch)
    scratchCapacity = Math.max(required, scratchCapacity * 2)
    scratch = alloc(scratchCapacity)
  }
  try {
    view().setUint16(slot, options.cols, true)
    view().setUint16(slot + 2, options.rows, true)
    view().setUint32(slot + 4, options.scrollbackBytes ?? 4 * 1024 * 1024, true)
    checked('ghostty_terminal_new', 0, slot + 8, slot)
    terminal = view().getUint32(slot + 8, true)
    checked('ghostty_render_state_new', 0, slot)
    render = view().getUint32(slot, true)
    checked('ghostty_render_state_row_iterator_new', 0, slot)
    iterator = view().getUint32(slot, true)
    checked('ghostty_render_state_row_cells_new', 0, slot)
    cells = view().getUint32(slot, true)
  } catch (cause) {
    dispose()
    throw cause
  }
  const get = (name: string, handle: number | bigint, key: number, width = 4) => {
    checked(name, handle, key, slot)
    return width === 1 ? view().getUint8(slot) : width === 2 ? view().getUint16(slot, true) : view().getUint32(slot, true)
  }
  const term = (key: number, width = 4) => get('ghostty_terminal_get', terminal, key, width)
  const state = (key: number, width = 4) => get('ghostty_render_state_get', render, key, width)
  const mode = (key: number) => {
    checked('ghostty_terminal_mode_get', terminal, key, slot)
    return view().getUint8(slot) !== 0
  }
  const modes = (): TerminalModes => {
    assertLive()
    return {
      alternate_screen: term(6) === 1,
      application_cursor: mode(1), application_keypad: mode(66),
      bracketed_paste: mode(2004), focus_events: mode(1004),
      mouse_tracking: mode(1003) ? 'motion' : mode(1002) ? 'drag' : mode(1000) || mode(9) ? 'click' : 'none',
      mouse_encoding: mode(1006) || mode(1016) ? 'sgr' : mode(1005) ? 'utf8' : 'default',
      kitty_keyboard: term(8, 1),
    }
  }
  const rgb = (pointer: number) => `#${[0, 1, 2].map((offset) => view().getUint8(pointer + offset).toString(16).padStart(2, '0')).join('')}`
  const paint = (pointer: number): TerminalRun['fg'] => {
    const tag = view().getUint32(pointer, true)
    return tag === 1 ? view().getUint8(pointer + 8) : tag === 2 ? rgb(pointer + 8) : undefined
  }
  const text = () => {
    const read = () => {
      view().setUint32(buffer, scratch, true)
      view().setUint32(buffer + 4, scratchCapacity, true)
      view().setUint32(buffer + 8, 0, true)
      return call('ghostty_render_state_row_cells_get', cells, 9, buffer)
    }
    let result = read()
    if (result === -3) { grow(view().getUint32(buffer + 8, true)); result = read() }
    if (result !== 0) return ' '
    const length = view().getUint32(buffer + 8, true)
    const value = decoder.decode(new Uint8Array(memory.buffer, scratch, length))
    return value === '' || value.startsWith('\0') ? ' ' : value
  }
  const link = (column: number, row: number): string | undefined => {
    view().setUint32(point, 1, true) // VIEWPORT, not ACTIVE: links must follow scrolling.
    view().setUint16(point + 8, column, true)
    view().setUint32(point + 12, row, true)
    view().setUint32(ref, 12, true)
    if (call('ghostty_terminal_grid_ref', terminal, point, ref) !== 0) return undefined
    let result = call('ghostty_grid_ref_hyperlink_uri', ref, scratch, scratchCapacity, slot)
    if (result === -3) {
      grow(view().getUint32(slot, true))
      result = call('ghostty_grid_ref_hyperlink_uri', ref, scratch, scratchCapacity, slot)
    }
    if (result !== 0) return undefined
    try {
      const raw = decoder.decode(new Uint8Array(memory.buffer, scratch, view().getUint32(slot, true)))
      if (raw === '' || /[\p{Cc}]/u.test(raw)) return undefined
      const url = new URL(raw)
      if ((url.protocol === 'http:' || url.protocol === 'https:') && url.hostname !== '') return url.href
      if (url.protocol === 'file:' && (url.hostname === '' || url.hostname === 'localhost') && url.pathname.startsWith('/')) return url.href
    } catch { /* Invalid UTF-8 and unsupported URLs retain text without a link. */ }
    return undefined
  }
  let previous: TerminalScreen | undefined
  let revision = 0
  return {
    write: (bytes) => {
      assertLive()
      grow(bytes.length)
      new Uint8Array(memory.buffer).set(bytes, scratch)
      call('ghostty_terminal_vt_write', terminal, scratch, bytes.length)
    },
    reset: () => { assertLive(); call('ghostty_terminal_reset', terminal) },
    resize: ({ cols, rows }) => { assertLive(); checked('ghostty_terminal_resize', terminal, cols, rows, 0, 0) },
    scroll: (delta) => {
      assertLive()
      view().setUint32(behavior, delta === 'bottom' ? 1 : 2, true)
      view().setInt32(behavior + 8, delta === 'bottom' ? 0 : delta, true)
      call('ghostty_terminal_scroll_viewport', terminal, behavior)
    },
    following: () => { assertLive(); return term(32, 1) !== 0 },
    modes,
    snapshot: (meta) => {
      assertLive()
      checked('ghostty_render_state_update', render, terminal)
      const columns = state(1, 2)
      const rows = state(2, 2)
      const dirty = state(3)
      const lines: TerminalLine[] = []
      view().setUint32(slot, iterator, true)
      checked('ghostty_render_state_get', render, 4, slot)
      let row = 0
      while (call('ghostty_render_state_row_iterator_next', iterator)) {
        const old = previous?.lines[row]
        const rowDirty = get('ghostty_render_state_row_get', iterator, 1, 1) !== 0
        if (row < 200) {
          if (old !== undefined && !rowDirty && dirty !== 2) lines.push(old)
          else {
            checked('ghostty_render_state_row_get', iterator, 2, slot)
            const rawRow = view().getBigUint64(slot, true)
            const wrapped = get('ghostty_row_get', rawRow, 2, 1) !== 0
            view().setUint32(slot, cells, true)
            checked('ghostty_render_state_row_get', iterator, 3, slot)
            const runs: { text: string; cells: number; style: RunStyle }[] = []
            let column = 0
            let bytes = 0
            let truncated = false
            while (call('ghostty_render_state_row_cells_next', cells)) {
              const x = column++
              if (truncated) continue
              checked('ghostty_render_state_row_cells_get', cells, 1, slot)
              const rawCell = view().getBigUint64(slot, true)
              const wide = get('ghostty_cell_get', rawCell, 3)
              if (wide === 2 || wide === 3) continue
              const count = wide === 1 ? 2 : 1
              view().setUint32(style, 72, true)
              checked('ghostty_render_state_row_cells_get', cells, 2, style)
              const content = get('ghostty_cell_get', rawCell, 2)
              let bg = paint(style + 24)
              if (content === 2) bg = get('ghostty_cell_get', rawCell, 10, 1)
              if (content === 3) { checked('ghostty_cell_get', rawCell, 11, slot); bg = rgb(slot) }
              const fg = paint(style + 8)
              const uri = get('ghostty_cell_get', rawCell, 7, 1) ? link(x, row) : undefined
              const pen: RunStyle = {
                ...(fg !== undefined ? { fg } : {}), ...(bg !== undefined ? { bg } : {}),
                ...(view().getUint8(style + 56) ? { bold: true } : {}),
                ...(view().getUint8(style + 58) ? { dim: true } : {}),
                ...(view().getUint8(style + 57) ? { italic: true } : {}),
                ...(view().getUint32(style + 64, true) ? { underline: true } : {}),
                ...(view().getUint8(style + 60) ? { inverse: true } : {}),
                ...(view().getUint8(style + 62) ? { strikethrough: true } : {}),
                ...(uri !== undefined ? { link: { uri } } : {}),
              }
              const value = view().getUint8(style + 61) ? ' '.repeat(count) : text()
              const length = utf8.encode(value).length
              if (bytes + length > 4096) { truncated = true; continue }
              bytes += length
              const last = runs.at(-1)
              if (last !== undefined && sameStyle(last.style, pen)) { last.text += value; last.cells += count }
              else runs.push({ text: value, cells: count, style: pen })
            }
            while (runs.length > 0) {
              const last = runs.at(-1)!
              if (last.style.bg === undefined && !last.style.inverse && !last.style.underline && !last.style.strikethrough) {
                const trimmed = last.text.replace(/ +$/, '')
                last.cells -= last.text.length - trimmed.length
                last.text = trimmed
              }
              if (last.text !== '') break
              runs.pop()
            }
            const line: TerminalLine = {
              row, wrapped, redacted: false, truncated,
              text: runs.map((run) => run.text).join('').replace(/ +$/, ''),
              runs: runs.map((run) => ({ ...run.style, text: run.text, cells: run.cells })),
            }
            lines.push(old !== undefined && sameLine(old, line) ? old : line)
          }
        }
        checked('ghostty_render_state_row_set', iterator, 0, zero)
        row++
      }
      const hasCursor = state(14, 1) !== 0
      const cursorStyle = state(10)
      const cursor: TerminalScreen['cursor'] = {
        row: hasCursor ? state(16, 2) : 0, column: hasCursor ? state(15, 2) : 0,
        visible: state(11, 1) !== 0 && hasCursor,
        style: cursorStyle === 0 ? 'bar' : cursorStyle === 2 ? 'underline' : 'block',
        blinking: state(12, 1) !== 0,
      }
      checked('ghostty_terminal_get', terminal, 12, slot)
      const title = decoder.decode(new Uint8Array(memory.buffer, view().getUint32(slot, true), view().getUint32(slot + 4, true))) || meta.fallbackTitle
      const currentModes = modes()
      checked('ghostty_render_state_set', render, 0, zero)
      if (previous !== undefined && previous.rows === rows && previous.columns === columns &&
          previous.terminal_id === meta.terminalId && previous.runtime_incarnation === meta.incarnation && previous.title === title &&
          lines.length === previous.lines.length && lines.every((line, index) => line === previous!.lines[index]) &&
          Object.keys(cursor).every((key) => cursor[key as keyof typeof cursor] === previous!.cursor[key as keyof typeof cursor]) &&
          Object.keys(currentModes).every((key) => currentModes[key as keyof TerminalModes] === previous!.modes[key as keyof TerminalModes])) return previous
      previous = {
        kind: 'terminal-screen', terminal_id: Schema.decodeUnknownSync(TerminalId)(meta.terminalId),
        runtime_incarnation: meta.incarnation, rows, columns,
        revision: Schema.decodeUnknownSync(TerminalScreen.fields.revision)(String(++revision)),
        next_sequence: 0, truncated: rows > 200, lines, cursor, title, modes: currentModes,
      }
      return previous
    },
    dispose,
  }
}

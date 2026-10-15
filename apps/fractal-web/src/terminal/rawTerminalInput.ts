/**
 * Input policy of the raw terminal: which keys belong to the terminal, how composed and inserted
 * text is admitted, and how a paste is bracketed, confirmed or refused. Bytes come from the native
 * Ghostty encoder and travel as DATA frames on the raw PTY socket; nothing here queues or replays.
 */

import type { TerminalModes } from '@smalltalk/st3-client/schema'

import type { BrowserTerminalKey, TerminalKeyEncoder } from './ghosttyKeyboard.ts'

/** Above this many UTF-8 bytes a paste asks for confirmation before it is sent. */
export const pasteConfirmBytes = 4 * 1024
/** A paste larger than this is refused locally; it would not be one honest terminal write. */
export const pasteMaxBytes = 64 * 1024
/** Inserted or composed text up to this many UTF-8 bytes on one line goes straight to the terminal. */
export const directTextMaxBytes = 128

const bracketMarkers = ['\x1b[200~', '\x1b[201~']

/**
 * Clipboard line endings become the carriage return a terminal Enter sends, and embedded
 * bracketed-paste markers are removed so pasted text cannot end the bracket early and run as keys.
 * Removal repeats until stable: deleting one marker must not splice a new one together.
 */
export const normalizePaste = (text: string) => {
  let normalized = text.replaceAll('\r\n', '\r')
  for (let previous = ''; previous !== normalized; ) {
    previous = normalized
    for (const marker of bracketMarkers) normalized = normalized.replaceAll(marker, '')
  }
  return normalized
}

/**
 * Whether inserted text (an IME commit, dictation or autocomplete) must take the paste path:
 * any line break or other control character, or more than `directTextMaxBytes`.
 */
export const isBulkText = (text: string) =>
  [...text].some((char) => {
    const code = char.codePointAt(0) ?? 0
    return code < 0x20 || code === 0x7f
  }) || new TextEncoder().encode(text).length > directTextMaxBytes

/** What to do with one piece of text headed for the terminal. */
export type TerminalAdmission =
  | { readonly _tag: 'Send'; readonly bytes: Uint8Array }
  | { readonly _tag: 'Confirm'; readonly bytes: Uint8Array }
  | { readonly _tag: 'Refuse'; readonly notice: string }

/** A paste is encoded for the modes it was pasted under (bracketed or not), then sized. */
export const admitPaste = ({
  encoder,
  text,
  modes,
}: {
  readonly encoder: TerminalKeyEncoder
  readonly text: string
  readonly modes: TerminalModes
}): TerminalAdmission => {
  const bytes = encoder.paste({ text: normalizePaste(text), modes })
  if (bytes.length > pasteMaxBytes)
    return { _tag: 'Refuse', notice: `This paste is larger than ${pasteMaxBytes / 1024} KB. Nothing was sent.` }
  return bytes.length > pasteConfirmBytes ? { _tag: 'Confirm', bytes } : { _tag: 'Send', bytes }
}

/**
 * One admission policy for every text source that is not a key press: a short single line goes
 * straight to the terminal; anything with a line break or control character, or longer, takes the
 * paste path with its bracketing, confirmation and cap.
 */
export const admitText = (options: {
  readonly encoder: TerminalKeyEncoder
  readonly text: string
  readonly modes: TerminalModes
}): TerminalAdmission =>
  isBulkText(options.text)
    ? admitPaste(options)
    : { _tag: 'Send', bytes: options.encoder.text(options.text) }

/** The fields of a DOM keyboard event the router reads. */
export interface KeyIntent {
  readonly key: string
  readonly code: string
  readonly ctrlKey: boolean
  readonly metaKey: boolean
  readonly shiftKey: boolean
  readonly altKey: boolean
  readonly isComposing: boolean
  readonly keyCode: number
}

/**
 * Who owns one key press:
 * - `compose`: the IME is composing; the browser keeps the key.
 * - `leave`: Ctrl-\ hands keyboard focus back to the page.
 * - `navigate`: Tab while the terminal is focused but not typing moves focus on, so the
 *   terminal is never a keyboard trap.
 * - `copy` / `paste` / `find` / `browser`: the platform or the pane's find keeps the key.
 * - `encode`: the native encoder turns it into terminal bytes.
 */
export type KeyRoute = 'compose' | 'leave' | 'navigate' | 'copy' | 'paste' | 'find' | 'browser' | 'encode'

export const routeKey = ({
  event,
  typing,
  hasSelection,
}: {
  readonly event: KeyIntent
  /** The input target at the cursor holds focus (not just the terminal region). */
  readonly typing: boolean
  /** A non-empty text selection lies inside the terminal. */
  readonly hasSelection: boolean
}): KeyRoute => {
  if (event.isComposing || event.keyCode === 229) return 'compose'
  const letter = event.key.toLowerCase()
  if (event.ctrlKey && !event.metaKey && (event.key === '\\' || event.code === 'Backslash')) return 'leave'
  if (!typing && event.key === 'Tab') return 'navigate'
  if ((event.ctrlKey || event.metaKey) && letter === 'f') return 'find'
  if ((event.ctrlKey || event.metaKey) && letter === 'v') return 'paste'
  // Cmd-C always copies; Ctrl-C copies a selection and is otherwise the terminal's interrupt.
  if (letter === 'c' && (event.metaKey || (event.ctrlKey && (event.shiftKey || hasSelection)))) return 'copy'
  // Command shortcuts belong to the browser and the OS, as they do in native macOS terminals.
  if (event.metaKey) return 'browser'
  return 'encode'
}

/** Reduce a DOM keyboard event to the encoder's key description. */
export const browserTerminalKey = (
  event: Pick<KeyboardEvent, 'code' | 'key' | 'repeat' | 'shiftKey' | 'ctrlKey' | 'altKey' | 'metaKey' | 'getModifierState'>,
  action: 'press' | 'release',
  layout: ReadonlyMap<string, string> | undefined,
): BrowserTerminalKey => ({
  code: event.code,
  key: event.key,
  action: action === 'press' && event.repeat ? 'repeat' : action,
  shiftKey: event.shiftKey,
  ctrlKey: event.ctrlKey,
  altKey: event.altKey,
  metaKey: event.metaKey,
  capsLock: event.getModifierState('CapsLock'),
  numLock: event.getModifierState('NumLock'),
  altGraph: event.getModifierState('AltGraph'),
  unshifted: layout?.get(event.code),
})

/** What a `beforeinput` event means once composition has been accounted for. */
export type InsertDecision =
  /** Leave the event to the browser (deletions, formatting, composition updates). */
  | { readonly _tag: 'Pass' }
  /** Prevent default; the text, when present, is admitted to the terminal. */
  | { readonly _tag: 'Take'; readonly text: string | undefined }
  /** Prevent default and tell the user dropped text is not sent. */
  | { readonly _tag: 'Drop' }

/**
 * IME composition bookkeeping for the hidden input at the cursor. A composition's text is
 * admitted once at `compositionend`; browsers that also fire a trailing `beforeinput` with the
 * same committed text (Safari, some Chromium IMEs) do not send it twice.
 */
export const makeCompositionGate = () => {
  let composing = false
  let committed: string | undefined
  return {
    composing: () => composing,
    start: () => {
      composing = true
      committed = undefined
    },
    /** Returns the committed text to admit, if any. */
    end: (data: string): string | undefined => {
      composing = false
      committed = data
      return data === '' ? undefined : data
    },
    /** Any non-composing key press ends the window in which a duplicate commit can arrive. */
    key: () => {
      committed = undefined
    },
    beforeInput: (event: {
      readonly inputType: string
      readonly data: string | null
      readonly isComposing: boolean
    }): InsertDecision => {
      if (event.inputType === 'insertFromDrop') return { _tag: 'Drop' }
      if (event.isComposing || composing) return { _tag: 'Pass' }
      if (event.inputType !== 'insertText' && event.inputType !== 'insertFromComposition') return { _tag: 'Pass' }
      const duplicate = event.data === committed
      committed = undefined
      return { _tag: 'Take', text: duplicate || event.data === null ? undefined : event.data }
    },
  }
}

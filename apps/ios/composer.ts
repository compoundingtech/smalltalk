// What the message box does around a send: how long the text may be, what to say when a send is
// refused, and how the text comes back. Plain functions, so they are tested without the app.
import { INLINE_MAX_BYTES, utf8Bytes } from '@smalltalk/st3-views';

/** How a send that did not go ended: `unconfirmed` means st may have taken it. */
export type SendFailure = { reason: string; unconfirmed: boolean };

/** The longest message in UTF-8 bytes this st advertises, or undefined when it says nothing (an older st). */
export function messageLimit(caps: { limits?: { max_message_bytes?: number } } | null | undefined): number | undefined {
  const limit = caps?.limits?.max_message_bytes;
  return typeof limit === 'number' && limit > 0 ? limit : undefined;
}

export type DraftSize = {
  bytes: number;
  /** Past the advertised limit: Send is refused until the text is shorter. */
  over: boolean;
  /** What to show beside the box, or null while the message is short enough not to matter. */
  note: string | null;
};

function size(bytes: number): string {
  if (bytes < 1024) return `${bytes} bytes`;
  const kib = bytes / 1024;
  return `${kib >= 100 ? Math.round(kib) : Math.round(kib * 10) / 10} KiB`;
}

/**
 * The draft's size against the limit st advertises, counted in UTF-8 bytes as st counts them. With no
 * advertised limit nothing is blocked here (st answers for itself and the text is kept). The count
 * appears once the message is long, so the limit is known before Send is pressed.
 */
export function draftSize(text: string, limit: number | undefined): DraftSize {
  if (limit === undefined) return { bytes: text.length > INLINE_MAX_BYTES ? utf8Bytes(text) : 0, over: false, note: null };
  // Characters are at most four bytes, so a short string cannot be long, nor one of 1/4 the limit past it.
  if (text.length <= INLINE_MAX_BYTES / 4) return { bytes: 0, over: false, note: null };
  const bytes = utf8Bytes(text);
  if (bytes > limit) return { bytes, over: true, note: `Too long to send: ${size(bytes)}, and the limit is ${size(limit)}. Shorten it or split it into two messages. Your text is kept.` };
  if (bytes > INLINE_MAX_BYTES) return { bytes, over: false, note: `${size(bytes)} of ${size(limit)}` };
  return { bytes, over: false, note: null };
}

/**
 * The text back in the box after a send that did not go. Words typed while it was in flight stay:
 * they follow the text that came back. Nothing is ever dropped.
 */
export function restoreDraft(current: string, failed: string): string {
  if (!failed.trim()) return current;
  if (!current.trim()) return failed;
  if (current.includes(failed)) return current;
  return `${failed}\n\n${current}`;
}

/** Why a send was refused, in words for the person; st's own sentence where it already says it. */
export function sendReason(reason: string, unconfirmed: boolean): string {
  if (unconfirmed) return `${reason}. It may have been sent: look in the conversation first. Sending again will not send it twice.`;
  // An st that predates long messages refuses with advice the phone cannot follow.
  if (/cannot exceed 4 ?KiB|post a document first/i.test(reason)) return 'This st only takes messages up to 4 KiB. Shorten the message, or update st. Your text is kept.';
  return `${reason}. Your text is kept.`;
}

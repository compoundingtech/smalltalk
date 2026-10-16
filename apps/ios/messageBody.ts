// Reading the whole text of a long message: what the phone says when st cannot give it. The message
// shows its start either way; these are the words for why the rest is not there.
import { ClientError, plainError } from '../../clients/typescript/st3-client';

/** What a read of a message's whole text answered. */
export type WholeTextAnswer = { text: string; complete?: boolean };

/** The text to show, or why only the start can be. */
export type WholeText = { text: string; note?: string } | { text?: undefined; note: string };

const PARTIAL = 'Only the start of this message can be shown: the machine that holds the rest could not be reached just now. Try again in a moment.';

/** The answer of `message-body.get` as the reader shows it. */
export function wholeText(answer: WholeTextAnswer): WholeText {
  if (answer.complete === false) return { text: answer.text, note: PARTIAL };
  return { text: answer.text };
}

/** Why the whole text could not be read, in words for the person. */
export function wholeTextFailure(error: unknown): string {
  if (error instanceof ClientError) {
    switch (error.response.code) {
      // st words this one itself: it names the machine and says only the start remains.
      case 'blob-expired': return error.response.message || 'The full text of this message expired and was removed after its retention. Only the start remains.';
      case 'blob-not-found': return 'The full text of this message is no longer on the machine that took it. Only the start remains.';
      case 'remote-unavailable': return `${plainError(error)}, so only the start of this message can be shown. Try again in a moment.`;
      case 'not-found': return 'This message is gone.';
      case 'forbidden': return 'You may not read this message.';
      default: return plainError(error);
    }
  }
  return plainError(error);
}

// Answering something that waits on the person (a decision, an ask, a gate) must end in what the
// person would call true: if the answer landed, the app says so, however the reply came back.
//
// An answer is sent once with its own key. When st's reply is lost, or st says it may still
// complete (`applied: "unknown"`), the identical request is sent again (the same key, so st
// returns its first answer and never records a second). When st refuses with a stale fence, or the
// reply never arrives for good, the app asks whether the thing still waits: if it does not, the
// answer is done (this one landed, or someone answered it first) and a repeat submit is a success,
// not a failure. A refusal while it still waits is shown as the refusal it is.
import { notApplied, outcomeUnknown } from '../../clients/typescript/st3-client';

export type AnswerPorts<R> = {
  /** A fresh request with its own key, fenced to a snapshot read now. */
  build(): Promise<R>;
  send(request: R): Promise<unknown>;
  /** Whether the thing still waits: true, false (answered, closed or gone), or null when st cannot say. */
  stillWaiting(): Promise<boolean | null>;
  wait(ms: number): Promise<void>;
};

export type AnswerOutcome =
  | { ok: true; how: 'sent' | 'already' }
  | { ok: false; error: unknown };

/** How many requests a stale fence may cost before the refusal is shown. */
export const ANSWER_TRIES = 8;
/** How long to wait before sending the identical request again after a lost reply. */
export const REPEAT_AFTER_MS = 1000;

export async function submitAnswer<R>(ports: AnswerPorts<R>): Promise<AnswerOutcome> {
  let last: unknown;
  let wait = 50;
  for (let attempt = 0; attempt < ANSWER_TRIES; attempt++) {
    let request: R;
    try { request = await ports.build(); } catch (error) { return { ok: false, error }; }
    try { await ports.send(request); return { ok: true, how: 'sent' }; } catch (error) { last = error; }
    if (outcomeUnknown(last)) {
      // st may have taken it: the identical request gets st's first answer.
      await ports.wait(REPEAT_AFTER_MS);
      try { await ports.send(request); return { ok: true, how: 'sent' }; } catch (error) { last = error; }
    }
    // Whatever went wrong, an answer that is no longer waited for is an answer that landed.
    if ((await settled(ports)) === true) return { ok: true, how: 'already' };
    // Only a refusal that guarantees nothing was applied is tried again with a new request.
    if (!notApplied(last) || attempt + 1 >= ANSWER_TRIES) return { ok: false, error: last };
    await ports.wait(wait);
    wait = Math.min(wait * 2, 2000);
  }
  return { ok: false, error: last };
}

/** True when the thing no longer waits; false while it does or when st cannot say. */
async function settled<R>(ports: AnswerPorts<R>): Promise<boolean> {
  try { return (await ports.stillWaiting()) === false; } catch { return false; }
}

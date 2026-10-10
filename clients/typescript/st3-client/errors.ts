// What st's errors mean to a person, and whether trying again may help. The Rust client has the
// same rules (ClientError::plain and is_transient in crates/st3-client), so every client says
// the same thing and retries the same things.
import { ClientError } from './Client.generated.ts';

type Code = ClientError['response']['code'];

/** An error st sent (its code and message) in words a person reads: no codes. */
export function plainMessage(code: Code | string | undefined, message: string): string {
  const host = message.split(/\s+/).find(word => word.startsWith('host/'))?.slice('host/'.length).replace(/[^A-Za-z0-9]+$/, '');
  switch (code) {
    case 'stale-fence': return 'st changed while this was on its way, so it was not applied';
    case 'page-cursor-expired':
    case 'cursor-gap': return 'the list changed while it was being read';
    case 'remote-unavailable': return host ? `${host} cannot be reached right now` : 'the host this lives on cannot be reached right now';
    case 'rate-limited': return 'st asked to slow down for a moment';
    case 'runtime-authority-indeterminate': return 'st cannot tell yet which host runs this';
    case 'idempotency-conflict': return 'this was already sent once in another form, so it was not sent again';
    case 'unsupported-capability': return `this st does not do that yet (${message})`;
    case 'not-found': return `it is gone: ${message}`;
    case 'forbidden': return `not allowed: ${message}`;
    case 'internal': return `st hit a problem: ${message}`;
    case 'terminal-ended': return 'the terminal ended: its process exited';
    case 'terminal-unavailable': return 'the terminal cannot be reached right now';
    default: return message;
  }
}

/**
 * What st said of whether it applied the request, when it said: 'none' (nothing was queued, so a
 * fresh request is safe) or 'unknown' (it may still complete: send the identical request again,
 * with the same key). Any other answer, or none, says nothing. The Rust client has `applied()`.
 */
export function appliedAnswer(error: unknown): 'none' | 'unknown' | undefined {
  if (!(error instanceof ClientError)) return undefined;
  const applied = (error.response.details as Record<string, unknown> | undefined)?.applied;
  return applied === 'none' || applied === 'unknown' ? applied : undefined;
}

/**
 * Whether the request may have been taken without the person hearing so: its answer was lost (a
 * network failure, a deadline) or st said `applied: "unknown"`. Such a request is repeated
 * unchanged, never rebuilt with a new key, id or signature nonce.
 */
export function outcomeUnknown(error: unknown): boolean {
  if (error instanceof ClientError) return appliedAnswer(error) === 'unknown';
  return isTransient(error);
}

/** Any error as a person reads it. */
export function plainError(error: unknown): string {
  if (appliedAnswer(error) === 'unknown') return 'st may have taken it and has not confirmed it yet; sending it again is safe';
  if (error instanceof ClientError) return plainMessage(error.response.code, error.response.message);
  if (error instanceof Error) {
    if (/network request failed|failed to fetch/i.test(error.message)) return 'st cannot be reached right now';
    if (/timed? ?out|deadline/i.test(error.message)) return 'st took too long to answer; it may or may not have done it';
    return error.message;
  }
  return String(error);
}

const TRANSIENT = new Set<string>(['stale-fence', 'page-cursor-expired', 'cursor-gap', 'remote-unavailable', 'rate-limited', 'runtime-authority-indeterminate', 'terminal-unavailable', 'internal']);

/** Whether an error st sent on a stream may clear if asked again: a race, a host out of reach, a load spike. A frame without a code (a subscription limit, say) may too. */
export function isTransientCode(code: string | undefined): boolean {
  return !code || TRANSIENT.has(code);
}

/** Whether the same request may succeed if tried again after a short wait. */
export function isTransient(error: unknown): boolean {
  if (error instanceof ClientError) return error.response.retryable || TRANSIENT.has(error.response.code);
  // A request that never reached st, or whose answer was lost.
  return error instanceof TypeError || (error instanceof Error && /network|fetch|timed? ?out|deadline/i.test(error.message));
}

/** Whether st refused in a way that guarantees nothing was applied, so a fresh request is safe. */
export function notApplied(error: unknown): boolean {
  return error instanceof ClientError && (error.response.code === 'stale-fence' || error.response.code === 'rate-limited' || appliedAnswer(error) === 'none');
}

/**
 * Run `attempt` until it succeeds, its error is not one `retryable` accepts, or `tries` run out,
 * waiting a little longer each time (50 ms doubling, up to 2 s). Each try builds its request
 * afresh, so a fence read inside it is current.
 */
export async function retryTransient<T>(tries: number, attempt: (n: number) => Promise<T>, retryable: (error: unknown) => boolean = isTransient): Promise<T> {
  let wait = 50;
  for (let n = 0; ; n++) {
    try { return await attempt(n); }
    catch (error) {
      if (n + 1 >= tries || !retryable(error)) throw error;
      await new Promise(resolve => setTimeout(resolve, wait));
      wait = Math.min(wait * 2, 2000);
    }
  }
}

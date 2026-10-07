import type { ClientDiagnosticEvent, ClientDiagnosticsAck, ClientDiagnosticsBatch } from '../../clients/typescript/st3-client';
import { redactDiagnosticEvent } from './diagnosticsRedaction';

export const DIAGNOSTICS_KEY = 'st3.client-diagnostics.v1';
export const QUEUE_REPORTS = 128, QUEUE_BYTES = 512 * 1024, MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000;
export const BATCH_REPORTS = 32, BATCH_BYTES = 128 * 1024;
export type QueueState = { version: 1; events: ClientDiagnosticEvent[]; failures: number; retry_at: number };
const empty = (): QueueState => ({ version: 1, events: [], failures: 0, retry_at: 0 });
// Every persisted character is ASCII after redaction, so string length equals UTF-8 bytes.
export const boundQueue = (state: QueueState, now: number): QueueState => {
  const seen = new Set<string>();
  const events = state.events.filter(event => {
    if (event.captured_at_unix_ms <= now - MAX_AGE_MS || event.captured_at_unix_ms > now + 5 * 60 * 1000 || seen.has(event.event_id)) return false;
    seen.add(event.event_id); return true;
  }).sort((a, b) => a.captured_at_unix_ms - b.captured_at_unix_ms).slice(-QUEUE_REPORTS);
  const bounded = { ...state, events };
  while (events.length && JSON.stringify(bounded).length > QUEUE_BYTES) events.shift();
  return bounded;
};
export const decodeQueue = (text: string | null, now: number): QueueState => {
  if (!text || text.length > QUEUE_BYTES) return empty();
  try {
    const value = JSON.parse(text);
    if (value?.version !== 1 || !Array.isArray(value.events)) return empty();
    const events = value.events.slice(-QUEUE_REPORTS).flatMap((item: unknown) => { const event = redactDiagnosticEvent(item); return event ? [event] : []; });
    const failures = Number.isSafeInteger(value.failures) && value.failures >= 0 ? Math.min(16, value.failures) : 0;
    const retry_at = Number.isSafeInteger(value.retry_at) && value.retry_at >= 0 ? Math.min(now + 5 * 60 * 1000, value.retry_at) : 0;
    return boundQueue({ version: 1, events, failures, retry_at }, now);
  } catch { return empty(); }
};
export const diagnosticBatch = (events: ClientDiagnosticEvent[]): ClientDiagnosticsBatch => {
  const batch: ClientDiagnosticsBatch = { version: 1, events: [] };
  for (const event of events.slice(0, BATCH_REPORTS)) {
    if (JSON.stringify({ version: 1, events: [...batch.events, event] }).length > BATCH_BYTES) break;
    batch.events.push(event);
  }
  return batch;
};
export const retryDelay = (failures: number, random: number): number => Math.round(Math.min(300_000, 1000 * 2 ** Math.min(9, Math.max(0, failures - 1))) * (0.75 + Math.max(0, Math.min(1, random)) * 0.25));

type Storage = { read: () => Promise<string | null>; write: (text: string) => Promise<void> };
export type DiagnosticSender = (batch: ClientDiagnosticsBatch) => Promise<ClientDiagnosticsAck>;
/** One serialized durability lane; network I/O never holds up capture. Failed writes do not commit memory. */
export class DiagnosticsQueue {
  private state: QueueState = empty();
  private loaded = false;
  private lane: Promise<unknown> = Promise.resolve();
  private upload: Promise<number | undefined> | undefined;
  constructor(private storage: Storage, private now: () => number = Date.now, private random: () => number = Math.random) {}
  private serialized<T>(operation: () => Promise<T>): Promise<T> {
    const result = this.lane.then(async () => {
      if (!this.loaded) { this.state = decodeQueue(await this.storage.read(), this.now()); this.loaded = true; }
      return operation();
    });
    this.lane = result.catch(() => {});
    return result;
  }
  private async commit(state: QueueState): Promise<void> {
    const bounded = boundQueue(state, this.now());
    await this.storage.write(JSON.stringify(bounded));
    this.state = bounded;
  }
  /** Return only IDs durably present, for safe acknowledgement of the native queue. */
  enqueue(inputs: readonly unknown[]): Promise<string[]> {
    return this.serialized(async () => {
      const events = inputs.slice(-QUEUE_REPORTS).flatMap(input => { const event = redactDiagnosticEvent(input); return event ? [event] : []; });
      await this.commit({ ...this.state, events: [...this.state.events, ...events] });
      const retained = new Set(this.state.events.map(event => event.event_id));
      return events.filter(event => retained.has(event.event_id)).map(event => event.event_id);
    });
  }
  flush(send: DiagnosticSender, current: () => boolean = () => true): Promise<number | undefined> {
    if (this.upload) return this.upload.then(() => this.flush(send, current));
    const run = async (): Promise<number | undefined> => {
      const batch = await this.serialized(async () => {
        await this.commit(this.state); // Expiry is durable even when no upload is needed.
        if (!current() || this.state.retry_at > this.now()) return undefined;
        return diagnosticBatch(this.state.events);
      });
      if (!current()) return undefined;
      if (!batch) return this.state.retry_at;
      if (!batch.events.length) return undefined;
      try {
        const ack = await send(batch);
        const sent = new Set(batch.events.map(event => event.event_id));
        // Reject malformed/no-progress responses rather than tight-looping or deleting unsent IDs.
        if (!Array.isArray(ack.acknowledged_event_ids)) throw new TypeError('Invalid diagnostic acknowledgement');
        const acknowledged = new Set(ack.acknowledged_event_ids.filter(id => sent.has(id)));
        if (!acknowledged.size) throw new TypeError('Empty diagnostic acknowledgement');
        return await this.serialized(async () => {
          await this.commit({ ...this.state, events: this.state.events.filter(event => !acknowledged.has(event.event_id)), failures: 0, retry_at: 0 });
          return this.state.events.length ? this.now() : undefined;
        });
      } catch {
        return this.serialized(async () => {
          const failures = Math.min(16, this.state.failures + 1);
          const retry_at = this.now() + retryDelay(failures, this.random());
          await this.commit({ ...this.state, failures, retry_at });
          return retry_at;
        });
      }
    };
    this.upload = run().finally(() => { this.upload = undefined; });
    return this.upload;
  }
}

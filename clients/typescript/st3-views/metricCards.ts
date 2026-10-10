import type { HostFacts, Machine } from '@smalltalk/st3-client';

// Metric cards, as crates/st-surface/src/metrics.rs makes them for stui and Fractal. Both check
// fixtures/clients/metric-cards.json, so the web kit and the terminals cannot drift. Renderers
// resolve `tone` through their own palette.

export const METRICS_POLL_INTERVAL_MS = 2_000;
const STALE_AFTER_MS = 3 * METRICS_POLL_INTERVAL_MS;

export type CardKind = 'agents_running' | 'host_load';
export type CardTone = 'normal' | 'busy' | 'stale' | 'unknown';
export type Card = {
  id: string;
  kind: CardKind;
  /** The st resource the card is about (`machine/<name>`). */
  subject: string;
  label: string;
  value: string;
  tone: CardTone;
};

type MachineFacts = Pick<Machine, 'id' | 'host_id' | 'state' | 'occupancy'>;

/** Agents running, then load, for each machine; `facts` describes at most one of them. */
export function metricCards(machines: readonly MachineFacts[], facts: HostFacts | null, nowMs: number): Card[] {
  return machines.flatMap(machine => {
    const subject = machine.id;
    const reachable = machine.state === 'local' || machine.state === 'reachable';
    const agents: Card = {
      id: `${subject}#agents-running`, kind: 'agents_running', subject, label: 'agents',
      value: `${machine.occupancy.running_runtimes} running`, tone: reachable ? 'normal' : 'stale',
    };
    return [agents, { id: `${subject}#load-1m`, kind: 'host_load', subject, label: 'load 1m', ...load(machine, facts, nowMs) }];
  });
}

function load(machine: MachineFacts, facts: HostFacts | null, nowMs: number): Pick<Card, 'value' | 'tone'> {
  if (!facts || facts.host_id !== machine.host_id) return { value: 'not reported', tone: 'unknown' };
  const observed = Date.parse(facts.observed_at);
  if (Number.isNaN(observed)) return { value: 'unreadable time', tone: 'unknown' };
  if (facts.load.state === 'unknown') return { value: facts.load.reason, tone: 'unknown' };
  const { one_minute, cpus } = facts.load;
  const tone = Math.max(0, nowMs - observed) > STALE_AFTER_MS ? 'stale' : one_minute >= cpus ? 'busy' : 'normal';
  return { value: `${one_minute.toFixed(2)} / ${cpus} cpu`, tone };
}

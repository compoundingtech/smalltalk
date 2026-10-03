// What a new agent or shell can be, as stui's launcher offers it (crates/stui/src/ui/screens.rs:
// HARNESSES, EFFORTS, models, random_name), so the phone and stui start the same things.

import type { AgentCheckout, AgentCreateParameters } from '../../clients/typescript/st3-client';

/** The same safe simple branch name as st's CLI. */
export function agentBranch(name: string): string {
  const identity = name.replace(/^(?:agent\/)+/, '');
  let simple = identity.split('/').pop() ?? identity;
  if (!identity.includes('/') && simple.includes('.')) simple = simple.slice(simple.indexOf('.') + 1);
  return simple.replace(/[^a-zA-Z0-9_-]/gu, '-').replace(/^-+|-+$/g, '') || 'agent';
}

export function checkoutLabel(checkout?: AgentCheckout | null): string | undefined {
  return checkout ? `worktree · branch ${checkout.branch} · ${checkout.repository}` : undefined;
}

export const HARNESSES = ['claude', 'codex', 'omp', 'pi', 'opencode'] as const;
export type Harness = typeof HARNESSES[number];
export const EFFORTS = ['default', 'low', 'medium', 'high', 'xhigh'] as const;

/** The models offered for a harness; `default` lets the harness choose. */
export function models(harness: string): readonly string[] {
  switch (harness) {
    case 'claude': return ['default', 'claude-opus-5-5', 'claude-sonnet-5-5', 'claude-haiku-4-5', 'claude-fable-5-1'];
    case 'codex': return ['default', 'gpt-6-sol'];
    default: return ['default'];
  }
}

const FIRST = ['amber', 'brisk', 'calm', 'clever', 'dusky', 'eager', 'gentle', 'keen', 'lucky', 'merry', 'nimble', 'quiet', 'rapid', 'steady', 'sunny', 'witty'];
const SECOND = ['badger', 'comet', 'falcon', 'fern', 'harbor', 'heron', 'lantern', 'maple', 'otter', 'pebble', 'quartz', 'raven', 'sparrow', 'tide', 'willow', 'wren'];

/** A name nobody has to think of, `amber-otter`; the person can change it. */
export function randomName(random: () => number = Math.random): string {
  return `${FIRST[Math.floor(random() * FIRST.length)]}-${SECOND[Math.floor(random() * SECOND.length)]}`;
}

/** agent.create's parameters from the form; a `default` choice is left to the harness. */
export function agentParameters(form: { message: string; name: string; harness: Harness; model: string; effort: string; host?: string; repository?: string; branch?: string; base?: string; workspace?: string }): AgentCreateParameters {
  return {
    name: form.name.trim(),
    harness: form.harness,
    ...(form.model !== 'default' ? { model: form.model } : {}),
    ...(form.effort !== 'default' ? { effort: form.effort } : {}),
    ...(form.host ? { host: form.host } : {}),
    ...(form.repository?.trim() ? { repo: form.repository.trim(), branch: form.branch?.trim() || agentBranch(form.name.trim()), base: form.base?.trim() || 'origin/main' } : {}),
    ...(form.workspace?.trim() ? { workspace: form.workspace.trim() } : {}),
    ...(form.message.trim() ? { message: form.message.trim() } : {}),
  };
}

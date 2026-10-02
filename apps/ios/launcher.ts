// What a new agent or shell can be, as stui's launcher offers it (crates/stui/src/ui/screens.rs:
// HARNESSES, EFFORTS, models, random_name), so the phone and stui start the same things.

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
export function agentParameters(form: { message: string; name: string; harness: Harness; model: string; effort: string; host?: string }) {
  return {
    name: form.name.trim(),
    harness: form.harness,
    ...(form.model !== 'default' ? { model: form.model } : {}),
    ...(form.effort !== 'default' ? { effort: form.effort } : {}),
    ...(form.host ? { host: form.host } : {}),
    ...(form.message.trim() ? { message: form.message.trim() } : {}),
  };
}

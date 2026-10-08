// What a new shell is called, as stui names one (crates/stui/src/ui/screens.rs random_name), and
// how a checkout reads. The new-agent form is gone from both (Nathan, 2026-10-07).

import type { AgentCheckout } from '../../clients/typescript/st3-client';

export function checkoutLabel(checkout?: AgentCheckout | null): string | undefined {
  return checkout ? `worktree · branch ${checkout.branch} · ${checkout.repository}` : undefined;
}

const FIRST = ['amber', 'brisk', 'calm', 'clever', 'dusky', 'eager', 'gentle', 'keen', 'lucky', 'merry', 'nimble', 'quiet', 'rapid', 'steady', 'sunny', 'witty'];
const SECOND = ['badger', 'comet', 'falcon', 'fern', 'harbor', 'heron', 'lantern', 'maple', 'otter', 'pebble', 'quartz', 'raven', 'sparrow', 'tide', 'willow', 'wren'];

/** A name nobody has to think of, `amber-otter`; the person can change it. */
export function randomName(random: () => number = Math.random): string {
  return `${FIRST[Math.floor(random() * FIRST.length)]}-${SECOND[Math.floor(random() * SECOND.length)]}`;
}

import type { TerminalRoute } from '@smalltalk/st3-client';

const subject = (id: string) => id.replace(/^terminal\//, '');

/** Only a current gateway-authenticated route can receive the paired credential. */
export function terminalRoute(value: TerminalRoute, id: string, pin?: string): TerminalRoute {
  if (!value || typeof value.terminal_id !== 'string' || subject(value.terminal_id) !== subject(id)
    || typeof value.runtime_incarnation !== 'string' || !value.runtime_incarnation
    || typeof value.owner_host_id !== 'string' || !value.owner_host_id) {
    throw new Error('The gateway returned an invalid terminal route');
  }
  if (pin && value.runtime_incarnation !== pin) throw new Error('Terminal restarted; reopen it before sending input.');
  if (value.fabric && (typeof value.fabric.node !== 'string' || typeof value.fabric.service !== 'string'
    || !/^[a-f0-9]{64}$/.test(value.fabric.node)
    || !/^st3\/client\/[^\s]+$/.test(value.fabric.service) || value.fabric.service.length > 255)) {
    throw new Error('The gateway returned an invalid owner Fabric target');
  }
  return value;
}

export const fabricRouteLabel = (route: TerminalRoute) => `Fabric to ${route.owner_host_id.replace(/^host\//, '')}`;
export const daemonRouteLabel = (reason: string, remembered = false) => `via the daemon (Fabric: ${reason}${remembered ? ', not tried again yet' : ''})`;

/** Network failures are shared by this owner/target. Terminal or person refusals are not. */
export class TerminalRouteMemory {
  private readonly failures = new Map<string, { at: number; reason: string }>();
  constructor(private readonly now: () => number = Date.now) {}
  private key(route: TerminalRoute): string { return JSON.stringify([route.owner_host_id, route.fabric?.node, route.fabric?.service]); }
  failed(route: TerminalRoute, reason: string): void { this.failures.set(this.key(route), { at: this.now(), reason }); }
  reason(route: TerminalRoute): string | undefined {
    const key = this.key(route), failure = this.failures.get(key);
    if (!failure) return;
    if (this.now() - failure.at >= 60_000) { this.failures.delete(key); return; }
    return failure.reason;
  }
  succeeded(route: TerminalRoute): void { this.failures.delete(this.key(route)); }
}

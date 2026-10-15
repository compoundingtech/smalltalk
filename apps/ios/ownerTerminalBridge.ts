import { St3Client, type TerminalRoute } from '../../clients/typescript/st3-client';
import { TerminalRouteMemory } from '@smalltalk/st3-views';
import { gatewayFetch } from './gatewayFetch';
import { closeFabricTarget, fabricTargetStats, openFabricTarget } from './modules/st-fabric';
import type { FabricTarget } from './fabricTarget';
import type { TerminalRouting } from './terminalRoute';

// Feed/carrier replacement still shares the single native owner slot.
let ownerTasks = Promise.resolve();

/** Paired credentials go only to a descriptor supplied by this gateway, never a pasted target. */
export function ownerTerminalRouting(options: {
  gatewayUrl: string; credential: string; clientName: string;
  built(): boolean; fallback(): boolean; gatewayFabric(): FabricTarget | null;
  memory?: TerminalRouteMemory;
}): TerminalRouting {
  const create = (url: string, signal?: AbortSignal, cleanup = false) => new St3Client({
    baseUrl: url, credential: () => options.credential, client: options.clientName,
    fetchImpl: gatewayFetch(async (input, init) => {
      if (!cleanup) return fetch(input, { ...init, signal });
      // Teardown is an external network action and cannot hold a native target forever.
      const controller = new AbortController();
      const deadline = setTimeout(() => controller.abort(), 4000);
      try { return await fetch(input, { ...init, signal: controller.signal }); }
      finally { clearTimeout(deadline); }
    }),
  });
  return {
    built: options.built, fallback: options.fallback, memory: options.memory ?? new TerminalRouteMemory(),
    describe: async (id, signal) => (await create(options.gatewayUrl, signal).terminalRoute(id)).value,
    open: async (route: TerminalRoute, signal) => {
      if (!route.fabric) throw new Error('The owner advertises no Fabric client service');
      const gateway = options.gatewayFabric();
      const reused = gateway?.node === route.fabric.node && gateway.service === route.fabric.service;
      const opening = ownerTasks.then(async () => {
        if (signal.aborted) throw new Error('Terminal route cancelled');
        // Opening the listener is a local path, before any request dials the owner.
        const target = reused ? null : await openFabricTarget(route.fabric!.node, route.fabric!.service).catch(error => {
          throw Object.assign(error instanceof Error ? error : new Error(String(error)), { ownerUnreachable: false });
        });
        if (signal.aborted) {
          if (target) await closeFabricTarget(target.handle);
          throw new Error('Terminal route cancelled');
        }
        return target;
      });
      ownerTasks = opening.then(() => {}, () => {});
      const target = await opening;
      const url = target?.url ?? options.gatewayUrl;
      let released = false;
      return {
        client: create(url, signal), cleanup: create(url, undefined, true),
        release: async after => {
          if (released) return;
          released = true;
          if (target) {
            ownerTasks = ownerTasks.then(() => after).catch(() => {}).then(() => closeFabricTarget(target.handle)).catch(() => {});
            await ownerTasks;
          }
        },
        failure: async error => {
          const response = (error as { response?: { code?: string } })?.response;
          // A client service, person scope or terminal refusal is local to this terminal, so do not mark
          // the host unreachable. Transport admission is diagnosed on this specific target.
          let admission = false;
          if (target && !response) {
            try {
              const attempts = (await fabricTargetStats(target.handle)).attempts;
              admission = Array.isArray(attempts) && attempts.at(-1)?.result === 'refused';
            } catch { /* The HTTP/network failure still supplies its own cause. */ }
          }
          return { reason: admission ? 'The owner refused Fabric transport access for this phone' : error instanceof Error ? error.message : String(error), unreachable: !response };
        },
      };
    },
  };
}

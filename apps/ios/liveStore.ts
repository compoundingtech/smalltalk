// The live graph through the paired gateway, as a `Store`. The port of stui's `ui::live`:
// refreshes replace data in place and never empty a list or a conversation while the fresh
// copy is on its way; what only the open screen needs (a conversation, a launch preview, the
// message behind an item) is fetched while that screen shows it; nothing runs in the background.

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AppState } from 'react-native';
import * as Crypto from 'expo-crypto';
import { ClientError, St3Client, type Agent as GraphAgent, type Attention as GraphAttention, type Fence, type LaunchVariant, type Message, type Mission as GraphMission, type Page, type Resource, type Runtime, type TimelineEntry, type Work } from '../../clients/typescript/st3-client';
import type { Attention, Entry, Load, Mission, MissionPreview } from './clientView';
import { listCollectionPages, withConcurrency } from './collectionPages';
import { demoWorld } from './demoStore';
import { ForegroundGate } from './foreground';
import { gatewayFetch } from './gatewayFetch';
import { clock, conversation } from './harnessConversation';
import { coalescedRefreshDelay } from './refreshFlight';
import { aboutText, aboutTitle, chatTarget, reconcilePending, withPending, type PendingSend } from './screenModel';
import { isSnapshotChurn, listSessionPages } from './sessionView';
import type { CardAction, MissionAction, Store } from './store';
import { names, notLoaded, openAttention, preview, sessionFor, world, type Collection, type Graph, type GraphMachine, type GraphSession, type MessageBody } from './worldAdapter';

export function errorText(error: unknown): string {
  return error instanceof ClientError ? `${error.response.code}: ${error.message}` : error instanceof Error ? error.message : String(error);
}
const isStale = (error: unknown) => error instanceof ClientError && error.response.code === 'stale-fence';
const actionId = () => `action/ios-${Crypto.randomUUID()}`;
const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

function pick<T extends Resource['kind']>(pages: Array<{ value: Page }>, kind: T): Extract<Resource, { kind: T }>[] {
  return pages.flatMap(page => page.value.items.filter((item): item is Extract<Resource, { kind: T }> => item.kind === kind));
}

/** How long an open conversation may go without a refetch when no event says it changed. */
const CONVERSATION_FALLBACK_MS = 45_000;
/** After a send, a reply is likely soon: refetch every few seconds for two minutes. */
const HOT_INTERVAL_MS = 3_000, HOT_FOR_MS = 120_000;
const EVENT_REFRESH_MS = 10_000;
const TIMELINE_KEEP = 400;

// st has no worktree resources yet; the Worktrees tab shows the demo's, labelled as such.
let demoWorktrees: ReturnType<typeof demoWorld>['worktrees'] | undefined;
const worktrees = () => (demoWorktrees ??= demoWorld().worktrees);

const emptyGraph = (): Graph => ({
  actor: '', hostId: null,
  attention: notLoaded(), agents: notLoaded(), missions: notLoaded(), work: notLoaded(), machines: notLoaded(), runtimes: notLoaded(), sessions: notLoaded(),
});

/** The newest timeline entries, merged with what was already read (streaming entries revise in place). */
async function loadTimeline(client: St3Client, session: string, previous: TimelineEntry[] | undefined, pageSize: number): Promise<TimelineEntry[]> {
  const known = previous?.length ? Math.max(...previous.map(entry => entry.sequence)) : -1;
  const found = new Map((previous ?? []).map(entry => [entry.id, entry]));
  let cursor: string | undefined;
  for (let page = 0; page < 6; page++) {
    const result = (await client.timelineList(session, { limit: pageSize, cursor })).value;
    for (const entry of result.items) found.set(entry.id, entry);
    const reachedKnown = known >= 0 && result.items.some(entry => entry.sequence <= known);
    if (reachedKnown || !result.page.has_more || !result.page.next_cursor) break;
    cursor = result.page.next_cursor;
  }
  return [...found.values()].sort((a, b) => a.sequence - b.sequence).slice(-TIMELINE_KEEP);
}

export function useLiveStore({ url, credential, leave }: { url: string; credential: string; leave: () => Promise<void> }): Store {
  const client = useMemo(() => new St3Client({ baseUrl: url, credential: () => credential, fetchImpl: gatewayFetch() }), [url, credential]);
  const [graph, setGraph] = useState<Graph>(emptyGraph);
  const graphRef = useRef(graph);
  graphRef.current = graph;
  const [live, setLive] = useState(false);
  const [offline, setOffline] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [snoozed, setSnoozed] = useState<ReadonlySet<string>>(new Set());
  const [timelines, setTimelines] = useState<Record<string, TimelineEntry[]>>({});
  const timelinesRef = useRef(timelines);
  timelinesRef.current = timelines;
  const [mail, setMail] = useState<Record<string, Message[]>>({});
  const [conversationErrors, setConversationErrors] = useState<Record<string, string>>({});
  const [previews, setPreviews] = useState<Record<string, Load<MissionPreview>>>({});
  const [bodies, setBodies] = useState<Record<string, MessageBody>>({});
  const [pending, setPending] = useState<PendingSend[]>([]);
  const [now, setNow] = useState(Date.now());
  const foreground = useRef(new ForegroundGate(AppState.currentState));
  const limitRef = useRef(30);
  const cursorRef = useRef<string | null>(null);
  const refreshing = useRef<Promise<void> | null>(null);
  const lastRefreshAt = useRef(0);
  const watched = useRef(new Map<string, number>());
  const hot = useRef(new Map<string, number>());
  const fetchedAt = useRef(new Map<string, number>());
  const stale = useRef(new Set<string>());
  const requestedAttention = useRef(new Set<string>());
  const [watchVersion, setWatchVersion] = useState(0);

  useEffect(() => {
    const subscription = AppState.addEventListener('change', state => foreground.current.update(state));
    return () => subscription.remove();
  }, []);

  const refresh = useCallback(async () => {
    if (!foreground.current.active) return;
    if (refreshing.current) return refreshing.current;
    const run = (async () => {
      for (let attempt = 0; attempt < 4; attempt++) {
        try {
          const capabilities = await client.capabilities();
          const limit = Math.min(capabilities.value.limits.max_page_items, 30);
          limitRef.current = limit;
          cursorRef.current ??= capabilities.value.event_cursor;
          const tasks = [
            () => listCollectionPages(options => client.attentionList(options), limit, 10).then(result => pick(result.pages, 'attention')),
            () => listCollectionPages(options => client.agentsList(options), limit).then(result => pick(result.pages, 'agent').filter(agent => (agent as GraphAgent & { operational?: { layer?: string } }).operational?.layer !== 'history')),
            () => listCollectionPages(options => client.missionsList(options), limit).then(result => pick(result.pages, 'mission')),
            () => listCollectionPages(options => client.workList(options), limit).then(result => pick(result.pages, 'work')),
            () => listCollectionPages(options => client.machinesList(options), limit).then(result => result.pages.flatMap(page => page.value.items as unknown as GraphMachine[]).filter(item => (item as { kind?: string }).kind === 'machine')),
            () => listCollectionPages(options => client.runtimesList(options), limit).then(result => pick(result.pages, 'runtime')),
            () => listSessionPages(options => client.sessionsList(options), limit) as unknown as Promise<GraphSession[]>,
          ];
          const settled = await withConcurrency<unknown[]>(tasks, 4);
          const churn = settled.find((result): result is PromiseRejectedResult => result.status === 'rejected' && isSnapshotChurn(result.reason));
          if (churn) throw churn.reason;
          const hostId = capabilities.snapshot.host_id;
          setGraph(previous => {
            const read = <T,>(index: number, before: Collection<T>): Collection<T> => {
              const result = settled[index];
              if (result.status === 'fulfilled') return { items: result.value as T[], loaded: true };
              // Keep what was shown; a collection that never loaded says why.
              return before.loaded ? before : { items: [], loaded: false, error: errorText(result.reason) };
            };
            return {
              actor: capabilities.value.session_actor,
              hostId: hostId || previous.hostId,
              attention: read<GraphAttention>(0, previous.attention),
              agents: read<GraphAgent>(1, previous.agents),
              missions: read<GraphMission>(2, previous.missions),
              work: read<Work>(3, previous.work),
              machines: read<GraphMachine>(4, previous.machines),
              runtimes: read<Runtime>(5, previous.runtimes),
              sessions: read<GraphSession>(6, previous.sessions),
            };
          });
          setLive(true);
          setOffline(null);
          setNow(Date.now());
          lastRefreshAt.current = Date.now();
          return;
        } catch (error) {
          if (isSnapshotChurn(error) && attempt < 3) { await sleep(200 * 2 ** attempt); continue; }
          setLive(false);
          setOffline(errorText(error));
          return;
        }
      }
    })();
    refreshing.current = run;
    try { await run; } finally { refreshing.current = null; }
  }, [client]);

  // One read on start and on every return to the foreground; nothing follows the gateway in
  // the background.
  useEffect(() => { void refresh(); }, [refresh]);
  useEffect(() => foreground.current.subscribe(active => { if (active) void refresh(); }), [refresh]);
  useEffect(() => {
    if (!offline) return;
    const timer = setInterval(() => { if (foreground.current.active) void refresh(); }, 15_000);
    return () => clearInterval(timer);
  }, [offline, refresh]);

  // Follow graph events while foregrounded: a changed conversation refetches at once, anything
  // else reloads the lists at most once per ten seconds.
  useEffect(() => {
    if (!live) return;
    let running = true;
    (async () => {
      while (running) {
        if (!foreground.current.active) { await foreground.current.untilActive(); continue; }
        const after = cursorRef.current;
        if (!after) { await sleep(1000); continue; }
        try {
          const page = await client.eventsList({ after, limit: 100, wait_ms: 25_000 });
          if (!running) return;
          cursorRef.current = page.value.resume_cursor;
          if (!page.value.items.length) continue;
          const touched = new Set(page.value.items.flatMap(event => event.resource_ids));
          for (const agent of watched.current.keys()) {
            const session = sessionFor(graphRef.current, agent);
            if (touched.has(agent) || (session && touched.has(session))) stale.current.add(agent);
          }
          const wait = coalescedRefreshDelay(lastRefreshAt.current, Date.now(), EVENT_REFRESH_MS);
          if (wait) await sleep(wait);
          if (running && foreground.current.active) await refresh();
        } catch (error) {
          if (!running) return;
          if (error instanceof ClientError && error.response.code === 'cursor-gap') {
            cursorRef.current = null;
            for (const agent of watched.current.keys()) stale.current.add(agent);
            await refresh();
          }
          await sleep(1000);
        }
      }
    })();
    return () => { running = false; };
  }, [client, live, refresh]);

  const fetchConversation = useCallback(async (agent: string) => {
    fetchedAt.current.set(agent, Date.now());
    stale.current.delete(agent);
    const session = sessionFor(graphRef.current, agent);
    const tasks: Promise<void>[] = [];
    if (session) {
      tasks.push(loadTimeline(client, session, timelinesRef.current[agent], Math.min(limitRef.current * 3, 100))
        .then(entries => {
          setTimelines(current => ({ ...current, [agent]: entries }));
          setConversationErrors(current => { const { [agent]: _, ...rest } = current; return rest; });
        })
        .catch(error => { setConversationErrors(current => ({ ...current, [agent]: errorText(error) })); }));
    }
    if (agent.startsWith('agent/')) {
      tasks.push(listCollectionPages(options => client.messagesList({ ...options, actor: agent, history: true }), limitRef.current, 2)
        .then(result => {
          const messages = pick(result.pages, 'message');
          const ids = new Set(messages.map(message => message.id));
          setPending(current => reconcilePending(current, agent, ids));
          setMail(current => ({ ...current, [agent]: messages }));
        })
        .catch(() => { /* the transcript alone is still the conversation */ }));
    }
    await Promise.all(tasks);
  }, [client]);

  // Watched conversations refetch when an event names them, every few seconds after a send,
  // and otherwise on a slow fallback.
  useEffect(() => {
    if (!live) return;
    const timer = setInterval(() => {
      if (!foreground.current.active) return;
      const at = Date.now();
      for (const agent of watched.current.keys()) {
        const interval = (hot.current.get(agent) ?? 0) > at ? HOT_INTERVAL_MS : CONVERSATION_FALLBACK_MS;
        if (stale.current.has(agent) || at - (fetchedAt.current.get(agent) ?? 0) >= interval) void fetchConversation(agent);
      }
    }, 1000);
    return () => clearInterval(timer);
  }, [fetchConversation, live, watchVersion]);

  const watchConversation = useCallback((agent: string) => {
    watched.current.set(agent, (watched.current.get(agent) ?? 0) + 1);
    setWatchVersion(version => version + 1);
    if (!fetchedAt.current.has(agent) || Date.now() - (fetchedAt.current.get(agent) ?? 0) > HOT_INTERVAL_MS) void fetchConversation(agent);
    return () => {
      const count = (watched.current.get(agent) ?? 1) - 1;
      if (count <= 0) watched.current.delete(agent); else watched.current.set(agent, count);
    };
  }, [fetchConversation]);

  const watchAttention = useCallback((id: string) => {
    const item = openAttention(graphRef.current).find(candidate => candidate.id === id);
    if (!item || requestedAttention.current.has(id)) return () => {};
    requestedAttention.current.add(id);
    if (item.attention_kind === 'launch-approval') {
      const name = (item.mission_id ?? item.title).replace(/^mission\//, '');
      void client.launchVariantsList(item.source_id, { limit: Math.min(limitRef.current, 50) }).then(result => {
        const variants = pick([result], 'launch-variant');
        const latest = variants.reduce<LaunchVariant | undefined>((best, variant) => (!best || variant.ordinal > best.ordinal ? variant : best), undefined);
        let loaded: Load<MissionPreview>;
        if (!latest) loaded = { state: 'failed', value: 'The planner has not proposed a mission yet.' };
        else if (!latest.normalized_mission || !Object.keys(latest.normalized_mission).length) {
          const reasons = latest.diagnostics.slice(0, 3).map(diagnostic => (diagnostic as { message?: unknown }).message).filter((message): message is string => typeof message === 'string');
          loaded = { state: 'failed', value: [`The planner's latest candidate (${latest.status}) has no preview, so there is no mission to show yet.`, ...reasons.map(reason => `• ${reason}`)].join('\n') };
        } else loaded = { state: 'ready', value: preview(name, latest.normalized_mission as Record<string, unknown>) };
        setPreviews(current => ({ ...current, [id]: loaded }));
      }, error => {
        setPreviews(current => ({ ...current, [id]: { state: 'failed', value: `Could not load the proposed mission: ${errorText(error)}` } }));
      });
    }
    if (item.attention_kind === 'unread-message') {
      void client.messagesGet(item.source_id).then(result => {
        if (result.value.kind !== 'message') return;
        const message = result.value;
        setBodies(current => ({ ...current, [id]: { from: message.from, title: message.title ?? null, content: message.content } }));
      }, () => { requestedAttention.current.delete(id); });
    }
    return () => {};
  }, [client]);

  /** A fenced message send: a fresh snapshot each time, and once more if the graph moved. */
  const sendMessage = useCallback(async (to: string, content: string, extra: { title?: string; in_reply_to?: string; session_id?: string }) => {
    let last: unknown;
    for (let attempt = 0; attempt < 2; attempt++) {
      const snapshot = (await client.capabilities()).snapshot.id;
      const id = actionId();
      try {
        const result = await client.messageSend({ id, idempotency_key: id, fence: { snapshot_id: snapshot, subject_revisions: {} }, parameters: { to, content, ...extra } });
        return result.value.affected_ids.find(affected => affected.startsWith('message/')) ?? null;
      } catch (error) {
        if (!isStale(error)) throw error;
        last = error;
      }
    }
    throw last ?? new Error('the graph kept changing; try again');
  }, [client]);

  /** Show a send at once, then keep st's message id so the copy gives way to the real one. */
  const sendPending = useCallback(async (agent: string, text: string, send: () => Promise<string | null>) => {
    const token = Crypto.randomUUID();
    setPending(current => [...current, { token, agent, text, at: clock(new Date().toISOString()), messageId: null, failed: null }]);
    hot.current.set(agent, Date.now() + HOT_FOR_MS);
    try {
      const messageId = await send();
      setPending(current => current.map(entry => entry.token === token ? { ...entry, messageId } : entry));
      stale.current.add(agent);
      return true;
    } catch (error) {
      setPending(current => current.map(entry => entry.token === token ? { ...entry, failed: errorText(error) } : entry));
      setNotice(`Not sent: ${errorText(error)}`);
      return false;
    }
  }, []);

  /** Carry out an action st offers on an attention item, against a fresh fence. */
  const attentionAction = useCallback(async (id: string, action: GraphAttention['actions'][number], reason?: string) => {
    let last: unknown;
    for (let attempt = 0; attempt < 2; attempt++) {
      const current = await client.attentionGet(id);
      if (current.value.kind !== 'attention') throw new Error('This item changed; look again');
      const item = current.value;
      if (item.person_id !== graphRef.current.actor || !item.actions.includes(action)) throw new Error('That action is no longer available');
      const fence: Fence = { snapshot_id: current.snapshot.id, subject_revisions: { [item.id]: item.revision } };
      if (action.startsWith('launch.')) {
        const launch = await client.launchesGet(item.source_id);
        if (launch.value.kind !== 'launch') throw new Error('The launch is gone');
        fence.snapshot_id = launch.snapshot.id;
        fence.subject_revisions[launch.value.id] = launch.value.revision;
      }
      const common = () => { const actionIdValue = actionId(); return { id: actionIdValue, idempotency_key: actionIdValue }; };
      try {
        switch (action) {
          case 'attention.resolve': await client.attentionResolve({ ...common(), fence, parameters: { attention_id: item.id, outcome: 'resolved' } }); return;
          case 'review.approve': await client.reviewApprove({ ...common(), fence, parameters: { target_id: item.source_id, ...(reason ? { reason } : {}) } }); return;
          case 'review.reject': await client.reviewReject({ ...common(), fence, parameters: { target_id: item.source_id, ...(reason ? { reason } : {}) } }); return;
          case 'message.read': await client.messageRead({ ...common(), fence, parameters: { target_id: item.source_id } }); return;
          case 'launch.cancel': await client.launchCancel({ ...common(), fence, parameters: { target_id: item.source_id } }); return;
          case 'launch.approve': {
            const variants = pick([(await client.launchVariantsList(item.source_id, { limit: Math.min(limitRef.current, 50) }))], 'launch-variant').filter(variant => variant.preview_token);
            const latest = variants.reduce<LaunchVariant | undefined>((best, variant) => (!best || variant.ordinal > best.ordinal ? variant : best), undefined);
            if (!latest?.preview_token) throw new Error('There is no current launch preview to approve');
            await client.launchApprove({ ...common(), fence: { ...fence, preview_token: latest.preview_token }, parameters: { launch_id: item.source_id, variant_id: latest.id } });
            return;
          }
          default: throw new Error(`This needs the st CLI for now: ${action}`);
        }
      } catch (error) {
        if (!isStale(error)) throw error;
        last = error;
      }
    }
    throw last;
  }, [client]);

  const worldValue = useMemo(() => {
    const graphNames = names(graph);
    const conversations: Record<string, Load<Entry[]>> = {};
    for (const agent of new Set([...Object.keys(timelines), ...Object.keys(mail), ...Object.keys(conversationErrors), ...watched.current.keys()])) {
      const timeline = timelines[agent], messages = mail[agent];
      if (!timeline && !messages) {
        conversations[agent] = conversationErrors[agent] ? { state: 'failed', value: `Could not load this conversation: ${conversationErrors[agent]}` }
          : agent.startsWith('session/') || sessionFor(graph, agent) ? { state: 'loading' }
            : { state: 'failed', value: 'This agent has no running session, so there is no transcript to show.' };
        continue;
      }
      conversations[agent] = { state: 'ready', value: conversation(timeline ?? [], (messages ?? []).map(message => ({ ...message, title: message.title ?? null })), graphNames) };
    }
    return world(graph, { conversations: withPending(conversations, pending), previews, bodies, live, offline, worktrees: worktrees() }, now);
    // watchVersion: a new watch shows "loading" at once.
  }, [graph, timelines, mail, conversationErrors, pending, previews, bodies, live, offline, now, watchVersion]);

  const act = useCallback(async (item: Attention, action: CardAction, text?: string) => {
    const kind = item.kind.kind;
    const reason = text?.trim();
    const offered = (name: GraphAttention['actions'][number]) => item.actions.includes(name);
    const run = async (name: GraphAttention['actions'][number], done: string, why?: string) => {
      if (!offered(name)) { setNotice(`st does not offer ${name} here`); return false; }
      try {
        setNotice('Sending…');
        await attentionAction(item.id, name, why);
        setNotice(done);
        void refresh();
        return true;
      } catch (error) { setNotice(`Failed: ${errorText(error)}`); return false; }
    };
    switch (`${kind}:${action}`) {
      case 'review:approve': return run('review.approve', 'Approved');
      case 'review:send-back': return run('review.reject', 'Sent back with your notes', reason);
      case 'launch:approve': return run('launch.approve', 'Approved; the mission starts');
      case 'launch:cancel': return run('launch.cancel', 'Launch cancelled');
      case 'fault:resolve': return run('attention.resolve', 'Marked resolved');
      case 'message:read': return run('message.read', 'Marked read');
      case 'revision:approve': case 'revision:reject':
        setNotice(`st does not let this app ${action} a revision yet · use st missions revision`);
        return false;
      case 'feedback:feedback': case 'feedback:approve':
        setNotice('st does not take feedback from this app yet: it needs the feedback gate mode on the daemon');
        return false;
      case 'launch:ask-changes': {
        if (!reason) return false;
        try {
          const found = await client.attentionGet(item.id);
          if (found.value.kind !== 'attention') throw new Error('This launch changed; look again');
          const launch = await client.launchesGet(found.value.source_id);
          if (launch.value.kind !== 'launch') throw new Error('The launch is gone');
          const id = actionId();
          await client.launchRevise({ id, idempotency_key: id, fence: { snapshot_id: launch.snapshot.id, subject_revisions: { [launch.value.id]: launch.value.revision } }, parameters: { launch_id: launch.value.id, feedback: reason } });
          setNotice('Sent your changes to the planner');
          return true;
        } catch (error) { setNotice(`Failed: ${errorText(error)}`); return false; }
      }
      case 'revision:ask-changes': {
        const target = chatTarget(worldValue, item);
        if (!reason || !target) { setNotice('Nobody to ask about this yet'); return false; }
        const session = graphRef.current.agents.items.find(agent => agent.id === target.id)?.current_session_id ?? undefined;
        return sendPending(target.id, reason, () => sendMessage(target.id, reason, { title: `Changes to: ${item.title}`, ...(session ? { session_id: session } : {}) }));
      }
      case 'message:reply': {
        if (!reason || item.kind.kind !== 'message') return false;
        const source = openAttention(graphRef.current).find(candidate => candidate.id === item.id)?.source_id;
        const to = item.kind.from;
        const ok = await sendPending(to, reason, () => sendMessage(to, reason, source ? { in_reply_to: source } : {}));
        if (ok) setNotice('Reply sent');
        return ok;
      }
      default:
        setNotice('This needs the st CLI for now');
        return false;
    }
  }, [attentionAction, client, refresh, sendMessage, sendPending, worldValue]);

  const discuss = useCallback(async (item: Attention, to: string, text: string) => {
    const session = graphRef.current.agents.items.find(agent => agent.id === to)?.current_session_id ?? undefined;
    const ok = await sendPending(to, text, () => sendMessage(to, aboutText(item, text), { title: aboutTitle(item), ...(session ? { session_id: session } : {}) }));
    if (ok) setNotice('Sent; the reply shows here and in their conversation');
    return ok;
  }, [sendMessage, sendPending]);

  const send = useCallback(async (agent: string, text: string) => {
    const session = graphRef.current.agents.items.find(candidate => candidate.id === agent)?.current_session_id ?? undefined;
    return sendPending(agent, text, () => sendMessage(agent, text, session ? { session_id: session } : {}));
  }, [sendMessage, sendPending]);

  const missionAction = useCallback(async (mission: Mission, action: MissionAction) => {
    if (action !== 'cancel') {
      setNotice(`${action === 'retry' ? 'Retry the step' : 'Restart the agent'}: st does not let this app do this yet · use ${action === 'retry' ? 'st work retry STEP' : 'st agents start AGENT'}`);
      return;
    }
    try {
      const found = graphRef.current.missions.items.find(candidate => candidate.id === mission.id);
      const run = found?.runs[found.runs.length - 1];
      if (!found || !run) throw new Error(found ? 'It has no run' : 'That mission is gone');
      const generation = found.run_generations[run];
      if (!generation) throw new Error('st has not said which generation of the run is current');
      const snapshot = (await client.capabilities()).snapshot.id;
      const id = actionId();
      await client.missionCancel({ id, idempotency_key: id, fence: { snapshot_id: snapshot, subject_revisions: {}, mission_generation: generation }, parameters: { target_id: run, reason: `cancelled from the iOS app by ${graphRef.current.actor}` } });
      setNotice(`Cancelled ${run}`);
      void refresh();
    } catch (error) { setNotice(`Failed: ${errorText(error)}`); }
  }, [client, refresh]);

  return useMemo<Store>(() => ({
    mode: 'live',
    world: worldValue,
    snoozed,
    notice,
    clearNotice: () => setNotice(null),
    refresh,
    watchConversation,
    watchAttention,
    act,
    discuss,
    send,
    snooze: id => { setSnoozed(current => new Set([...current, id])); setNotice('Put off until later · this device only'); },
    missionAction,
    leave,
    connection: { gateway: url, person: graph.actor, status: offline ? `Offline: ${offline}` : live ? 'Connected' : 'Connecting…' },
  }), [worldValue, snoozed, notice, refresh, watchConversation, watchAttention, act, discuss, send, missionAction, leave, url, graph.actor, offline, live]);
}

import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { AppState } from 'react-native';
import AsyncStorage from '@react-native-async-storage/async-storage';
import * as SecureStore from 'expo-secure-store';
import * as Crypto from 'expo-crypto';
import { API_VERSION, ClientError, St3Client, notApplied, plainError, retryTransient, type Attention, type AttachmentInput, type Capabilities, type ConversationSearch, type Glass, type Launch, type LaunchVariant, type Mission, type Resource, type Snapshot, type TimelineEntry } from '../../clients/typescript/st3-client';
import { personAnswer } from './requestView';
import { isSnapshotChurn, listSessionPages, OLDER_PAGE, readOlder, type Conversation, type Older, type SessionView } from './sessionView';
import { emptyData, encodeProjectionCache, hydrateProjectionForPairedDevice, PROJECTION_CACHE_KEY, type Data } from './projectionCache';
import { listCollectionPages } from './collectionPages';
import { rememberBounded } from './boundedCache';
import { withFreshTerminalFence } from './terminalControls';
import { Feed } from './feed';
import { ForegroundGate } from './foreground';
import { gatewayFetch } from './gatewayFetch';
import { normalizeGatewayUrl } from './gatewayUrl';
import { tabOrder, type Tab } from './tabs';
import { fetch as expoFetch } from 'expo/fetch';
import { decodeBase64, encodeBase64, type Picked } from './images';

// Everything the screens share: the paired gateway, the one collections socket, the lists it keeps
// current, the lists a screen loads when it opens, and the actions. Screens follow a conversation
// or a terminal themselves, on this socket, while they are open.

export type OnDemand = 'launches' | 'machines' | 'devices' | 'sessions';
export type Status = 'setup' | 'connecting' | 'online' | 'offline';
export type Planner = 'codex' | 'claude' | 'pi' | 'omp' | 'opencode';

const URL_KEY = 'st3.gateway.url', ORDER_KEY = 'st3.tabs.order', CREDENTIAL_KEY = 'st3.device.credential';
// Glasses are an experiment (mission fleet/stui/glass): off unless the person turns them on here.
const GLASSES_KEY = 'st3.experiments.glasses';
// Simplified conversations: this phone's own choice, on unless turned off, never synced.
const SIMPLE_KEY = 'st3.conversation.simple';

/** An error as a person reads it: st's errors in plain words (the SDK's plainError). */
export function errorText(error: unknown): string {
  return plainError(error);
}
function currentAgent(agent: { operational?: { layer?: string } }) { return agent.operational?.layer !== 'history'; }
function items<K extends Resource['kind']>(page: { items: Resource[] }, kind: K): Extract<Resource, { kind: K }>[] {
  return page.items.filter((item): item is Extract<Resource, { kind: K }> => item.kind === kind);
}
function actionId() { return `action/ios-${Crypto.randomUUID()}`; }

function useAppStore() {
  const [order, setOrder] = useState<Tab[]>(tabOrder(null));
  const [url, setUrl] = useState(''), [urlDraft, setUrlDraft] = useState('');
  const [credential, setCredential] = useState<string | null>(null);
  const [data, setData] = useState<Data>(emptyData);
  const [truncated, setTruncated] = useState<Partial<Record<keyof Data, boolean>>>({});
  const [loadErrors, setLoadErrors] = useState<Partial<Record<keyof Data, string>>>({});
  const foreground = useRef(new ForegroundGate(AppState.currentState));
  const [feed, setFeed] = useState<Feed | null>(null), [connectionIssue, setConnectionIssue] = useState('');
  const [cachedHostId, setCachedHostId] = useState('');
  const [caps, setCaps] = useState<Capabilities | null>(null), [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const capsRef = useRef(caps);
  capsRef.current = caps;
  const [status, setStatus] = useState<Status>('setup');
  const [hasSynced, setHasSynced] = useState(false);
  const [error, setError] = useState(''), [pairingIssue, setPairingIssue] = useState(''), [busy, setBusy] = useState(false);
  const [historicalSessions, setHistoricalSessions] = useState<SessionView[]>([]);
  // The Agents tab shows a list or a tree, as stui's `t` toggles. Debug links can set it, and ask
  // the visible screen to scroll for screenshots.
  const [treeView, setTreeView] = useState(false);
  // Spaces are how stui works, so the tab is on unless this phone turned it off.
  const [glassesOn, setGlassesOn] = useState(true);
  const [simpleOn, setSimpleOn] = useState(true);
  const [glasses, setGlasses] = useState<Glass[]>([]), [glassesIssue, setGlassesIssue] = useState('');
  const [scrollRequest, setScrollRequest] = useState<{ y: number; at: number } | null>(null);
  const cachedActor = useRef(''), cacheSavedAt = useRef(0), cacheGeneration = useRef(0);
  const conversationCache = useRef(new Map<string, Conversation<TimelineEntry>>()), draftCache = useRef(new Map<string, string>());
  // Images messages carry, as data URIs, so a conversation scrolled back to does not read them again.
  const imageCache = useRef(new Map<string, Promise<string>>());
  const missionDetailCache = useRef(new Map<string, Mission>());
  const client = useMemo(() => url ? new St3Client({ baseUrl: url, credential: () => credential ?? undefined, fetchImpl: gatewayFetch() }) : null, [url, credential]);
  // Image bytes go up through Expo's fetch: React Native's cannot send a byte array as a body.
  const uploader = useMemo(() => url ? new St3Client({ baseUrl: url, credential: () => credential ?? undefined, fetchImpl: gatewayFetch(expoFetch as unknown as typeof fetch) }) : null, [url, credential]);

  useEffect(() => { Promise.allSettled([AsyncStorage.getItem(URL_KEY), AsyncStorage.getItem(ORDER_KEY), SecureStore.getItemAsync(CREDENTIAL_KEY), AsyncStorage.getItem(PROJECTION_CACHE_KEY)]).then(([u, o, c, p]) => {
    if (u.status === 'fulfilled' && u.value) { setUrl(u.value); setUrlDraft(u.value); }
    // A stored order may use earlier tab names; they still count.
    if (o.status === 'fulfilled' && o.value) { try { setOrder(tabOrder(JSON.parse(o.value))); } catch { /* use default */ } }
    if (c.status === 'fulfilled' && c.value) {
      if (u.status === 'fulfilled' && u.value && p.status === 'fulfilled') {
        const cache = hydrateProjectionForPairedDevice(p.value, u.value, true);
        if (cache) { setData(cache.data); setTruncated(Object.fromEntries(cache.truncated.map(key => [key, true]))); setHasSynced(true); setStatus('connecting'); cachedActor.current = cache.actor; cacheSavedAt.current = cache.savedAt; setCachedHostId(cache.hostId); }
      }
      setCredential(c.value);
    }
    if (c.status === 'rejected') setError('Secure credential storage is unavailable on this build.');
  }); }, []);

  function clearCaches() {
    cachedActor.current = ''; cacheSavedAt.current = 0;
    conversationCache.current.clear(); draftCache.current.clear(); missionDetailCache.current.clear();
    setData(emptyData); setTruncated({}); setHasSynced(false); setCachedHostId(''); setSnapshot(null);
  }
  async function clearCachedProjection() { cacheGeneration.current++; clearCaches(); await AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {}); }

  // Capabilities once per connection: limits, the session's actor, and what it may control.
  const loadCapabilities = useCallback(async () => {
    if (!client) return;
    try {
      const capability = await client.capabilities();
      if (cachedActor.current && cachedActor.current !== capability.value.session_actor) { clearCaches(); void AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {}); }
      cachedActor.current = capability.value.session_actor;
      setCaps(capability.value);
    } catch (e) { if (e instanceof ClientError && e.status >= 400 && e.status < 500) setError(errorText(e)); }
  }, [client]);

  // One collections socket per paired gateway and credential keeps attention, missions, and agents
  // current. It closes in the background and opens fresh, snapshots first, in the foreground.
  useEffect(() => {
    if (!client || !credential) { setStatus('setup'); return; }
    const generation = cacheGeneration.current;
    const current = () => generation === cacheGeneration.current;
    const opened = new Feed(client, {
      onWindow: (name, rows, hasMore, at) => {
        if (!current()) return;
        // Home decides what of attention to show, as stui does; agents drop only history.
        const shown = name === 'agents' ? (rows as Array<{ operational?: { layer?: string } }>).filter(currentAgent) : rows;
        setData(previous => ({ ...previous, [name]: shown }));
        setTruncated(previous => ({ ...previous, [name]: hasMore }));
        setLoadErrors(previous => { if (!(name in previous)) return previous; const rest = { ...previous }; delete rest[name]; return rest; });
        setSnapshot(at); setCachedHostId(at.host_id); setHasSynced(true);
      },
      onConnection: (state, issue) => {
        if (!current()) return;
        setStatus(state === 'live' ? 'online' : state === 'connecting' ? 'connecting' : 'offline');
        setConnectionIssue(issue ?? '');
        if (state === 'live') { setError(''); void loadCapabilities(); }
      },
      onWindowError: (name, message) => { if (current()) setLoadErrors(previous => ({ ...previous, [name]: message })); },
    }, foreground.current, actionId);
    // Paired: connecting from here on, even while the app waits to be active before it dials.
    setStatus(previous => previous === 'setup' ? 'connecting' : previous);
    setFeed(opened);
    return () => { opened.close(); setFeed(held => held === opened ? null : held); };
  }, [client, credential, loadCapabilities]);
  useEffect(() => {
    const subscription = AppState.addEventListener('change', state => foreground.current.update(state));
    return () => subscription.remove();
  }, []);

  // Glasses: followed on the feed's socket while the experiment is on and the gateway grants them.
  useEffect(() => { void AsyncStorage.getItem(GLASSES_KEY).then(value => setGlassesOn(value !== '0')).catch(() => {}); }, []);
  useEffect(() => { void AsyncStorage.getItem(SIMPLE_KEY).then(value => setSimpleOn(value !== '0')).catch(() => {}); }, []);
  // Version 1 glasses are splits of tab groups; an earlier member's glasses are a shape this app no longer reads.
  const glassesGranted = caps?.capabilities.some(capability => capability.id === 'glasses' && capability.version >= 1 && capability.state === 'granted') ?? false;
  useEffect(() => {
    if (!feed || !glassesOn || !glassesGranted) { setGlasses([]); return; }
    const follow = feed.followGlasses({ onGlasses: setGlasses, onIssue: setGlassesIssue });
    return () => follow.close();
  }, [feed, glassesOn, glassesGranted]);
  // Keep the last data for an offline start, saved at most every 10 s while it changes.
  useEffect(() => {
    if (!hasSynced || !snapshot || !caps) return;
    const timer = setTimeout(() => {
      cacheSavedAt.current = Date.now();
      const truncatedKeys = (Object.keys(truncated) as Array<keyof Data>).filter(key => truncated[key]);
      const encoded = encodeProjectionCache(url, caps.session_actor, snapshot.host_id, snapshot.store_index, data, cacheSavedAt.current, truncatedKeys);
      if (encoded) void AsyncStorage.setItem(PROJECTION_CACHE_KEY, encoded).catch(() => { cacheSavedAt.current = 0; });
    }, Math.max(0, cacheSavedAt.current + 10_000 - Date.now()));
    return () => clearTimeout(timer);
  }, [caps, data, hasSynced, snapshot, truncated, url]);

  // Launches, machines, devices, and sessions have no window: a screen loads them when it opens,
  // and again after an action there. A list that fails is named; the others still show.
  const loadLists = useCallback(async (keys: readonly OnDemand[]) => {
    if (!client || !keys.length) return;
    const generation = cacheGeneration.current;
    const limit = Math.min(capsRef.current?.limits.max_page_items ?? 30, 30);
    const read = (key: OnDemand): Promise<{ rows: Data[OnDemand]; truncated: boolean }> => {
      if (key === 'sessions') return listSessionPages(options => client.sessionsList(options), limit).then(rows => ({ rows, truncated: false }));
      const list = key === 'launches' ? client.launchesList.bind(client) : key === 'machines' ? client.machinesList.bind(client) : client.devicesList.bind(client);
      const kind = key === 'launches' ? 'launch' : key === 'machines' ? 'machine' : 'device';
      return listCollectionPages(options => list(options), limit).then(result => ({ rows: result.pages.flatMap(page => page.value.items.filter(item => (item as { kind: string }).kind === kind)) as unknown as Data[OnDemand], truncated: result.truncated }));
    };
    const results = await Promise.allSettled(keys.map(read));
    if (generation !== cacheGeneration.current) return;
    keys.forEach((key, index) => {
      const result = results[index];
      if (result.status === 'fulfilled') {
        setData(previous => ({ ...previous, [key]: result.value.rows }));
        setTruncated(previous => ({ ...previous, [key]: result.value.truncated }));
        setLoadErrors(previous => { if (!(key in previous)) return previous; const rest = { ...previous }; delete rest[key]; return rest; });
      } else if (!isSnapshotChurn(result.reason)) setLoadErrors(previous => ({ ...previous, [key]: errorText(result.reason) }));
    });
  }, [client]);

  /** A fence on a snapshot read just now: one read seconds ago has nearly always moved on a busy host. */
  async function fence(revisions: Record<string, string> = {}) {
    if (!client) throw new Error('Still connecting; try again in a moment.');
    const capability = await client.capabilities();
    return { snapshot_id: capability.snapshot.id, subject_revisions: revisions };
  }
  /** Run an action, trying again (with a fresh fence each time) while st refused it only because it raced a busy store. */
  async function runAction(action: () => Promise<unknown>, reload: readonly OnDemand[] = []): Promise<boolean> {
    if (status !== 'online') { setError('Still connecting; try again in a moment.'); return false; }
    setBusy(true);
    try { await retryTransient(8, action, notApplied); setError(''); await loadLists(reload); return true; } catch (e) { setError(errorText(e)); return false; } finally { setBusy(false); }
  }

  async function completePairing(gatewayClient: St3Client, id: string, code: string, gateway?: string) {
    const publicKey = Array.from(Crypto.getRandomBytes(32), b => b.toString(16).padStart(2, '0')).join('');
    const result = await gatewayClient.completePairing(id, { api_version: API_VERSION, code, device_public_key: publicKey });
    await clearCachedProjection();
    await SecureStore.setItemAsync(CREDENTIAL_KEY, result.value.credential, { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY });
    if (gateway) { await AsyncStorage.setItem(URL_KEY, gateway); setUrl(gateway); setUrlDraft(gateway); }
    setCredential(result.value.credential); setPairingIssue(''); setError('');
  }

  const actions = {
    async saveUrl() {
      const normalized = normalizeGatewayUrl(urlDraft);
      if (!normalized) { setError('Enter the paired gateway HTTPS URL, or http:// with a Tailscale address (100.x), a .local name, or a private LAN address (10.x, 172.16-31.x, 192.168.x).'); return; }
      if (normalized !== url) await clearCachedProjection();
      await AsyncStorage.setItem(URL_KEY, normalized); setUrl(normalized); setError('');
    },
    async pair(id: string, code: string): Promise<boolean> {
      if (!client || !id.trim() || !code.trim()) return false;
      setBusy(true);
      try { await completePairing(client, id.trim(), code.trim()); return true; } catch (e) { setError(errorText(e)); return false; } finally { setBusy(false); }
    },
    /** A Debug pairing link: the gateway, pairing id, and code all in one. */
    async pairFromLink(gatewayRaw: string, id: string, code: string) {
      const gateway = normalizeGatewayUrl(gatewayRaw);
      if (!gateway) return;
      setPairingIssue(''); setBusy(true);
      try { await completePairing(new St3Client({ baseUrl: gateway }), id, code, gateway); } catch (e) { setPairingIssue(`Pairing failed: ${errorText(e)}`); } finally { setBusy(false); }
    },
    async forget() {
      await SecureStore.deleteItemAsync(CREDENTIAL_KEY); await clearCachedProjection();
      setCredential(null); setCaps(null); setPairingIssue(''); setHistoricalSessions([]); setStatus('setup');
    },
    /** Complete a person step; `answer` is a structured request's named answer, by id. */
    async done(item: Attention, summary: string, answer?: string) {
      if (!client) return false;
      const typed = personAnswer(item.request, answer, summary);
      if (typeof typed === 'string') { setError(typed); return false; }
      return runAction(async () => { const id = actionId(); return client.workDone({ id, idempotency_key: id, fence: await fence({ [item.id]: item.revision }), parameters: { target_id: item.source_id, episode: item.episode || item.revision, summary, ...(typed ? { answer: typed } : {}) } }); });
    },
    /** An image a message carries, as a data URI; st reads it from the member that has it. */
    image(image: { sha256: string; message: string; mediaType: string }): Promise<string> {
      const known = imageCache.current.get(image.sha256);
      if (known) return known;
      if (!client || status !== 'online') return Promise.reject(new Error(status === 'online' ? 'not connected' : 'offline'));
      const reading = client.blob(image.sha256, image.message).then(bytes => `data:${image.mediaType};base64,${encodeBase64(bytes)}`);
      // A failed read is tried again next time it is shown.
      reading.catch(() => { if (imageCache.current.get(image.sha256) === reading) imageCache.current.delete(image.sha256); });
      rememberBounded(imageCache.current, image.sha256, reading, 24);
      return reading;
    },
    /** Send Small Talk to an agent, as stui does: fenced to a fresh snapshot, once more if it moved. */
    async send(to: string, content: string, sessionId?: string, tags?: string[], images: Picked[] = []): Promise<string | null> {
      if (!client || !uploader) return 'not connected';
      if (status !== 'online') return 'offline';
      // Images are kept on this member first; the message names them, and st fetches each from
      // here for a reader on another machine (docs/st3/attachments.md).
      const attachments: AttachmentInput[] = [];
      for (const image of images) {
        try {
          const kept = (await uploader.uploadBlob(decodeBase64(image.base64), image.mediaType)).value;
          attachments.push({ blob: kept.blob, media_type: image.mediaType, ...(image.name ? { name: image.name } : {}) });
        } catch (e) {
          if (e instanceof ClientError && e.status === 404) return 'the st this phone is paired with cannot carry images yet; it needs a newer st';
          return `the image could not be sent: ${errorText(e)}`;
        }
      }
      try {
        // Each try is a new request on a fresh fence, made only after st said the last applied nothing.
        await retryTransient(8, async () => {
          const id = actionId();
          await client.messageSend({ id, idempotency_key: id, fence: await fence(), parameters: { to, content, ...(sessionId ? { session_id: sessionId } : {}), ...(tags?.length ? { tags } : {}), ...(attachments.length ? { attachments } : {}) } });
        }, notApplied);
        return null;
      } catch (e) { return errorText(e); }
    },
    /** A plain shell, named as stui names one; its terminal id, or null with the reason shown. */
    async createTerminal(name: string): Promise<string | null> {
      if (!client) return null;
      let created: string | null = null;
      const done = await runAction(async () => {
        const id = actionId();
        const result = await client.terminalCreate({ id, idempotency_key: id, fence: await fence(), parameters: { name } });
        created = result.value.affected_ids?.find(affected => affected.startsWith('terminal/')) ?? null;
      });
      if (!done || !created) return null;
      // The shell starts a moment after st declares it: open it once st can show its screen.
      for (let tries = 0; tries < 30; tries++) {
        try { await client.terminalScreen(created); return created; } catch { await new Promise(resolve => setTimeout(resolve, 500)); }
      }
      return created;
    },
    /** A new agent with its first message; its id, or null with the reason shown. */
    async createAgent(parameters: { name: string; harness: string; model?: string; effort?: string; host?: string; message?: string }): Promise<string | null> {
      if (!client) return null;
      let created: string | null = null;
      const done = await runAction(async () => {
        const id = actionId();
        const result = await client.agentCreate({ id, idempotency_key: id, fence: await fence(), parameters: parameters as never });
        created = result.value.affected_ids?.find(affected => affected.startsWith('agent/')) ?? null;
      });
      return done ? created : null;
    },
    async createLaunch(parameters: { title: string; request: string; workspace: string; provider: Planner; model?: string; effort?: string }) {
      if (!client) return false;
      return runAction(async () => { const id = actionId(); await client.launchCreate({ id, idempotency_key: id, fence: await fence(), parameters: { title: parameters.title, request: parameters.request, target: { type: 'new-mission', mission_id: `mission/ios-${Crypto.randomUUID()}`, workspace: parameters.workspace }, provider: parameters.provider, ...(parameters.model ? { model: parameters.model } : {}), ...(parameters.effort ? { effort: parameters.effort } : {}) } }); }, ['launches']);
    },
    async reviseLaunch(launch: Launch, feedback: string) {
      if (!client) return false;
      return runAction(async () => { const id = actionId(); await client.launchRevise({ id, idempotency_key: id, fence: await fence({ [launch.id]: launch.revision }), parameters: { launch_id: launch.id, feedback } }); }, ['launches']);
    },
    async variants(launchId: string): Promise<LaunchVariant[]> {
      if (!client || status !== 'online') return [];
      try { const result = await client.launchVariantsList(launchId, { limit: Math.min(caps?.limits.max_page_items ?? 30, 30) }); setError(''); return items(result.value, 'launch-variant'); } catch (e) { setError(errorText(e)); return []; }
    },
    async preview(launch: Launch, variant: LaunchVariant) {
      if (!client) return false;
      return runAction(async () => { const id = actionId(); return client.launchPreview({ id, idempotency_key: id, fence: await fence({ [launch.id]: launch.revision, [variant.id]: variant.revision }), parameters: { launch_id: launch.id, variant_id: variant.id } }); }, ['launches']);
    },
    async approve(launch: Launch, variant: LaunchVariant) {
      if (!client || !variant.preview_token) return false;
      return runAction(async () => { const id = actionId(); return client.launchApprove({ id, idempotency_key: id, fence: { ...(await fence({ [launch.id]: launch.revision, [variant.id]: variant.revision })), preview_token: variant.preview_token! }, parameters: { launch_id: launch.id, variant_id: variant.id } }); }, ['launches']);
    },
    /** An agent names its runtimes; a runtime is read only when the person opens its terminal. */
    async runtimeTerminal(runtimeId: string): Promise<string | null> {
      if (!client || status !== 'online') return null;
      try {
        const runtime = await client.runtimesGet(runtimeId);
        if (runtime.value.kind !== 'runtime' || !runtime.value.terminal_id) { setError('This runtime has no terminal.'); return null; }
        return runtime.value.terminal_id;
      } catch (e) { setError(errorText(e)); return null; }
    },
    /** Each input takes a fresh terminal fence and refuses a changed runtime incarnation. */
    async terminalInput(terminalId: string, incarnation: string, mode: 'line' | 'key' | 'raw', value: string) {
      if (!client) throw new Error('not connected');
      await withFreshTerminalFence(client, terminalId, incarnation, terminalFence => {
        const id = actionId();
        return client.terminalInput({ id, idempotency_key: id, fence: terminalFence, parameters: { terminal_id: terminalId, mode, value } });
      });
    },
    /** Sizes the terminal to the phone; whoever attaches next may size it again. */
    async terminalResize(terminalId: string, incarnation: string, rows: number, columns: number) {
      if (!client) throw new Error('not connected');
      await withFreshTerminalFence(client, terminalId, incarnation, terminalFence => {
        const id = actionId();
        return client.terminalResize({ id, idempotency_key: id, fence: terminalFence, parameters: { terminal_id: terminalId, rows, columns } });
      });
    },
    /** The page of a session's timeline before `oldest`, continuing from `older`'s cursor. */
    async olderTimeline(sessionId: string, older: Older, oldest: TimelineEntry | undefined) {
      if (!client || status !== 'online') throw new Error('offline');
      try {
        return await readOlder(async cursor => {
          const found = (await client.timelineList(sessionId, { cursor, limit: OLDER_PAGE })).value;
          return { items: found.items, hasMore: found.page.has_more, cursor: found.page.next_cursor ?? undefined };
        }, older, oldest);
      } catch (e) { throw new Error(errorText(e)); }
    },
    /** What was said in conversations, as st's search finds it; a string says why not. */
    async searchConversations(text: string): Promise<ConversationSearch | string> {
      if (!client || status !== 'online') return 'offline';
      try { return (await client.conversationSearch(text, { limit: 20 })).value; } catch (e) { return errorText(e); }
    },
    async mission(id: string): Promise<Mission | null> {
      if (!client || status !== 'online') return missionDetailCache.current.get(id) ?? null;
      try {
        const result = await client.missionsGet(id);
        if (result.value.kind !== 'mission') return null;
        rememberBounded(missionDetailCache.current, id, result.value, 12);
        return result.value;
      } catch { return missionDetailCache.current.get(id) ?? null; }
    },
    async history() {
      if (!client || !caps) return;
      try { setHistoricalSessions((await listSessionPages(options => client.sessionsList(options), Math.min(caps.limits.max_page_items, 30), true)).filter(s => ['completed', 'failed', 'cancelled'].includes(s.state))); setError(''); } catch (e) { if (!isSnapshotChurn(e)) setError(errorText(e)); }
    },
    moveTab(tab: Tab, direction: -1 | 1) {
      const index = order.indexOf(tab), next = index + direction;
      if (next < 0 || next >= order.length) return;
      const updated = [...order];
      [updated[index], updated[next]] = [updated[next], updated[index]];
      setOrder(updated);
      void AsyncStorage.setItem(ORDER_KEY, JSON.stringify(updated));
    },
    reconnect() { feed?.reconnect(); },
    setSimpleOn(on: boolean) {
      setSimpleOn(on);
      void AsyncStorage.setItem(SIMPLE_KEY, on ? '1' : '0').catch(() => {});
    },
    setGlassesOn(on: boolean) {
      setGlassesOn(on);
      void AsyncStorage.setItem(GLASSES_KEY, on ? '1' : '0').catch(() => {});
    },
  };

  const knownHostId = snapshot?.host_id ?? cachedHostId;
  const gatewayMachineId = knownHostId ? `machine/${knownHostId.replace(/^host\//, '')}` : '';
  const gatewayHost = data.machines.find(m => m.id === gatewayMachineId)?.name ?? (knownHostId ? knownHostId.replace(/^host\//, '') : '');
  const canControlTerminal = caps?.capabilities.some(capability => capability.id === 'terminal.input' && capability.state === 'granted') ?? false;

  return {
    order, url, urlDraft, setUrlDraft, credential, data, truncated, loadErrors, feed, connectionIssue, caps, snapshot, status, hasSynced,
    error, setError, pairingIssue, setPairingIssue, busy, historicalSessions, conversationCache, draftCache, client,
    gatewayMachineId, gatewayHost, canControlTerminal, loadLists, actions,
    treeView, setTreeView, scrollRequest, requestScroll: (y: number) => setScrollRequest({ y, at: Date.now() }),
    glassesOn, glassesGranted, glasses, glassesIssue, simpleOn,
  };
}

export type Store = ReturnType<typeof useAppStore>;
const StoreContext = createContext<Store | null>(null);
export function StoreProvider({ children }: { children: ReactNode }) {
  const store = useAppStore();
  return <StoreContext.Provider value={store}>{children}</StoreContext.Provider>;
}
export function useStore(): Store {
  const store = useContext(StoreContext);
  if (!store) throw new Error('useStore outside StoreProvider');
  return store;
}

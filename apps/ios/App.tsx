import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ActivityIndicator, Alert, AppState, Linking, Pressable, SafeAreaView, ScrollView, StyleSheet, Text, TextInput, View } from 'react-native';
import AsyncStorage from '@react-native-async-storage/async-storage';
import * as SecureStore from 'expo-secure-store';
import * as Crypto from 'expo-crypto';
import { API_VERSION, ClientError, St3Client, type Attention, type Capabilities, type Device, type Launch, type LaunchVariant, type Message, type Mission, type Page, type Resource, type Runtime, type Snapshot, type TerminalScreen, type TimelineEntry, type Work } from '../../clients/typescript/st3-client';
import { conversationRows, isSnapshotChurn, isUnmanaged, isUnresolved, listSessionPages, loadRecentConversation, sessionDetail, sessionLabel, sessionMessagesFor, timelineText, type Conversation, type SessionView } from './sessionView';
import { emptyData, encodeProjectionCache, hydrateProjectionForPairedDevice, offlinePresentation, PROJECTION_CACHE_KEY, type Data, type MachineView } from './projectionCache';
import { listCollectionPages, settleCollections, withConcurrency, type CollectionResult } from './collectionPages';
import { agentLabel, agentTree } from './agentTree';
import { tabsChangedByProjectionEvents } from './projectionRefresh';
import { rememberBounded } from './boundedCache';
import { withFreshTerminalFence } from './terminalControls';
import { coalescedRefreshDelay, RefreshFlight } from './refreshFlight';
import { ForegroundGate } from './foreground';
import { gatewayFetch } from './gatewayFetch';
import { gatewayTransport, LAN_HTTP_WARNING, normalizeGatewayUrl } from './gatewayUrl';
import { agentHeaderDetail, agentHealth, ago, attentionActionLabel, attentionHeadline, attentionKindLabel, deviceDetail, deviceTitle, missionLabels, pingPresentation, queuedWorkSummary } from './presentation';

const tabs = ['Now', 'Chat', 'Control', 'Fleet'] as const;
type Tab = typeof tabs[number];
const emptyConversation: Conversation<TimelineEntry> = { entries: [], hasOlder: false, newestSequence: -1 };
const URL_KEY = 'st3.gateway.url', ORDER_KEY = 'st3.tabs.order', CREDENTIAL_KEY = 'st3.device.credential';
function items<K extends Resource['kind']>(page: Page, kind: K): Extract<Resource, { kind: K }>[] {
  return page.items.filter((item): item is Extract<Resource, { kind: K }> => item.kind === kind);
}
function collectionItems<K extends Resource['kind']>(collection: CollectionResult, kind: K): Extract<Resource, { kind: K }>[] {
  return collection.pages.flatMap(page => items(page.value, kind));
}
function errorText(error: unknown) { return error instanceof ClientError ? `${error.response.code}: ${error.message}` : error instanceof Error ? error.message : String(error); }
function missionDetail(mission: Mission, work: Work[]): string {
  const planned = mission.visualization?.nodes.filter(node => node.kind === 'step').length;
  const visible = work.filter(step => mission.runs.includes(step.mission_run_id));
  const current = visible.find(step => step.state === 'blocked') ?? visible.find(step => step.state === 'claimed' || step.state === 'ready') ?? visible[0];
  return `${mission.runs.length} runs${planned === undefined ? '' : ` · ${planned} planned steps`}${current ? ` · ${current.path} (${current.state})` : ''}`;
}
const missionGroups = ['Blocked', 'Waiting', 'Running', 'Drafts', 'Archive'] as const;
type MissionGroup = typeof missionGroups[number];
function missionGroup(mission: Mission, work: Work[]): MissionGroup {
  const states = work.filter(step => mission.runs.includes(step.mission_run_id)).map(step => step.state);
  if (states.includes('blocked')) return 'Blocked';
  if (states.includes('waiting')) return 'Waiting';
  if (mission.state === 'running' || mission.state === 'standing') return 'Running';
  if (mission.state === 'ready' || mission.state === 'draft') return 'Drafts';
  return 'Archive';
}
function Button({ label, onPress, disabled = false }: { label: string; onPress: () => void; disabled?: boolean }) {
  return <Pressable accessibilityRole="button" disabled={disabled} onPress={onPress} style={[styles.button, disabled && styles.disabled]}><Text style={styles.buttonText}>{label}</Text></Pressable>;
}
function Card({ title, detail, children }: { title: string; detail?: string; children?: React.ReactNode }) {
  return <View style={styles.card}><Text style={styles.cardTitle}>{title}</Text>{detail ? <Text style={styles.muted}>{detail}</Text> : null}{children}</View>;
}
export default function App() {
  const [order, setOrder] = useState<Tab[]>([...tabs]);
  const [active, setActive] = useState<Tab>(() => {
    const testTab = process.env.EXPO_PUBLIC_ST3_TEST_TAB;
    return __DEV__ && tabs.includes(testTab as Tab) ? testTab as Tab : 'Now';
  });
  const [url, setUrl] = useState(''), [urlDraft, setUrlDraft] = useState('');
  const [credential, setCredential] = useState<string | null>(null);
  const [pairingId, setPairingId] = useState(''), [pairingCode, setPairingCode] = useState('');
  const [data, setData] = useState<Data>(emptyData);
  const [truncated, setTruncated] = useState<Partial<Record<keyof Data, boolean>>>({});
  const [loadErrors, setLoadErrors] = useState<Partial<Record<keyof Data, string>>>({});
  const dataRef = useRef(data), truncatedRef = useRef(truncated);
  dataRef.current = data; truncatedRef.current = truncated;
  const foreground = useRef(new ForegroundGate(AppState.currentState)), [appActive, setAppActive] = useState(AppState.currentState === 'active');
  const [cachedHostId, setCachedHostId] = useState('');
  const [caps, setCaps] = useState<Capabilities | null>(null), [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const capsRef = useRef(caps);
  capsRef.current = caps;
  const [status, setStatus] = useState<'setup' | 'connecting' | 'online' | 'offline'>('setup');
  const [hasSynced, setHasSynced] = useState(false);
  const [error, setError] = useState(''), [pairingIssue, setPairingIssue] = useState(''), [busy, setBusy] = useState(false);
  const [expandedAttentionId, setExpandedAttentionId] = useState<string | null>(null);
  const [sessionId, setSessionId] = useState(''), [timeline, setTimeline] = useState<Conversation<TimelineEntry>>(emptyConversation), [composer, setComposer] = useState('');
  const [chatDetailOpen, setChatDetailOpen] = useState(false);
  const [showHistory, setShowHistory] = useState(false), [historicalSessions, setHistoricalSessions] = useState<SessionView[]>([]), [historyBusy, setHistoryBusy] = useState(false);
  const [terminalId, setTerminalId] = useState(''), [screen, setScreen] = useState<TerminalScreen | null>(null), [terminalIssue, setTerminalIssue] = useState('');
  const [terminalDraft, setTerminalDraft] = useState('');
  const [terminalActionNotice, setTerminalActionNotice] = useState('');
  const terminalIncarnation = useRef(''), terminalSending = useRef(false);
  const [reviewLaunch, setReviewLaunch] = useState(''), [variants, setVariants] = useState<LaunchVariant[]>([]);
  const [selectedMissionId, setSelectedMissionId] = useState(''), [showSystemMissions, setShowSystemMissions] = useState(false), [showPlanner, setShowPlanner] = useState(false);
  const [missionDetailView, setMissionDetailView] = useState<Mission | null>(null);
  const missionDetailCache = useRef(new Map<string, Mission>());
  const selectedMissionRevision = data.missions.find(m => m.id === selectedMissionId)?.revision;
  const [title, setTitle] = useState(''), [request, setRequest] = useState(''), [workspace, setWorkspace] = useState('');
  const [provider, setProvider] = useState<'codex' | 'claude' | 'pi' | 'omp' | 'opencode'>('codex');
  const [model, setModel] = useState(''), [effort, setEffort] = useState(''), [feedback, setFeedback] = useState('');
  const refreshing = useRef(new RefreshFlight()), snapshotRetry = useRef(0);
  const firstDataShown = useRef(false);
  const cachedActor = useRef(''), cachedIndex = useRef(-1), cacheSavedAt = useRef(0);
  const projectionEventCursor = useRef<string | null>(null);
  const dirtyProjectionTabs = useRef(new Set<Tab>());
  const lastNativeRefreshAt = useRef(0), lastEventRefreshAt = useRef(0);
  const cacheGeneration = useRef(0);
  const conversationCache = useRef(new Map<string, Conversation<TimelineEntry>>()), draftCache = useRef(new Map<string, string>());
  const chatScrollCache = useRef(new Map<string, number>()), scrollView = useRef<ScrollView>(null), currentScrollY = useRef(0);
  const client = useMemo(() => url ? new St3Client({ baseUrl: url, credential: () => credential ?? undefined, fetchImpl: gatewayFetch() }) : null, [url, credential]);

  useEffect(() => {
    if (active !== 'Chat' || !chatDetailOpen || !sessionId) return;
    const frame = requestAnimationFrame(() => scrollView.current?.scrollTo({ y: chatScrollCache.current.get(sessionId) ?? 0, animated: false }));
    return () => cancelAnimationFrame(frame);
  }, [active, chatDetailOpen, sessionId]);

  useEffect(() => { Promise.allSettled([AsyncStorage.getItem(URL_KEY), AsyncStorage.getItem(ORDER_KEY), SecureStore.getItemAsync(CREDENTIAL_KEY), AsyncStorage.getItem(PROJECTION_CACHE_KEY)]).then(([u, o, c, p]) => {
    if (u.status === 'fulfilled' && u.value) { setUrl(u.value); setUrlDraft(u.value); }
    if (o.status === 'fulfilled' && o.value) { try { const parsed: unknown = JSON.parse(o.value); if (Array.isArray(parsed) && parsed.length === 4 && tabs.every(t => parsed.includes(t))) setOrder(parsed as Tab[]); } catch { /* use default */ } }
    if (c.status === 'fulfilled' && c.value) {
      if (u.status === 'fulfilled' && u.value && p.status === 'fulfilled') {
        const cache = hydrateProjectionForPairedDevice(p.value, u.value, true);
        if (cache) { setData(cache.data); setTruncated(Object.fromEntries(cache.truncated.map(key => [key, true]))); setHasSynced(true); firstDataShown.current = true; setStatus('connecting'); cachedActor.current = cache.actor; cachedIndex.current = cache.storeIndex; cacheSavedAt.current = cache.savedAt; setCachedHostId(cache.hostId); }
      }
      setCredential(c.value);
    }
    if (c.status === 'rejected') setError('Secure credential storage is unavailable on this build.');
  }); }, []);
  useEffect(() => {
    if (!__DEV__) return;
    let activePairLink = '';
    async function handleDevPairLink(link: string | null) {
      if (!link) return;
      const parsed = new URL(link);
      if (parsed.hostname === 'tab') {
        const tab = parsed.pathname.replace(/^\//, '');
        if (tabs.includes(tab as Tab)) setActive(tab as Tab);
        return;
      }
      if (parsed.hostname === 'scroll') {
        const y = Number(parsed.searchParams.get('y'));
        if (Number.isFinite(y) && y >= 0) scrollView.current?.scrollTo({ y, animated: false });
        return;
      }
      if (parsed.hostname === 'mission') {
        const id = parsed.searchParams.get('id');
        if (id?.startsWith('mission/')) { setSelectedMissionId(id); setActive('Control'); }
        return;
      }
      if (parsed.hostname === 'session') {
        const id = parsed.searchParams.get('id');
        const terminal = parsed.searchParams.get('terminal');
        if (id?.startsWith('session/')) {
          setSessionId(id);
          setTimeline(conversationCache.current.get(id) ?? emptyConversation);
          setComposer(draftCache.current.get(id) ?? '');
          setTerminalId(terminal?.startsWith('terminal/') ? terminal : '');
          terminalIncarnation.current = '';
          setTerminalDraft('');
          setTerminalActionNotice('');
          setScreen(null);
          setTerminalIssue('');
          setChatDetailOpen(true);
          setActive('Chat');
        }
        return;
      }
      if (parsed.hostname !== 'pair') return;
      if (activePairLink === link) return;
      const gateway = normalizeGatewayUrl(parsed.searchParams.get('gateway') ?? '');
      const id = parsed.searchParams.get('id');
      const code = parsed.searchParams.get('code');
      if (!gateway || !id || !code) return;
      activePairLink = link;
      setPairingIssue('');
      setBusy(true);
      try {
        const publicKey = Array.from(Crypto.getRandomBytes(32), b => b.toString(16).padStart(2, '0')).join('');
        const result = await new St3Client({ baseUrl: gateway }).completePairing(id, { api_version: API_VERSION, code, device_public_key: publicKey });
        await clearCachedProjection();
        await SecureStore.setItemAsync(CREDENTIAL_KEY, result.value.credential, { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY });
        await AsyncStorage.setItem(URL_KEY, gateway);
        setUrl(gateway); setUrlDraft(gateway); setCredential(result.value.credential); setPairingIssue(''); setError('');
      } catch (e) { activePairLink = ''; setPairingIssue(`Pairing failed: ${errorText(e)}`); }
      finally { setBusy(false); }
    }
    void Linking.getInitialURL().then(handleDevPairLink);
    void handleDevPairLink(process.env.EXPO_PUBLIC_ST3_TEST_PAIR_LINK ?? null);
    const subscription = Linking.addEventListener('url', event => { void handleDevPairLink(event.url); });
    return () => subscription.remove();
  }, []);

  const refresh = useCallback(async () => {
    if (!client || !credential) { setStatus('setup'); return; }
    if (!foreground.current.active) return; // resynced when the app returns to the foreground
    const generation = cacheGeneration.current;
    if (!refreshing.current.start(generation)) return;
    setStatus(s => s === 'online' ? s : 'connecting');
    try {
      const capability = await client.capabilities(), limit = Math.min(capability.value.limits.max_page_items, 30);
      if (cachedActor.current && cachedActor.current !== capability.value.session_actor) {
        cachedActor.current = ''; cachedIndex.current = -1; cacheSavedAt.current = 0;
        projectionEventCursor.current = null; lastNativeRefreshAt.current = 0;
        dirtyProjectionTabs.current.clear();
        conversationCache.current.clear(); draftCache.current.clear(); chatScrollCache.current.clear(); missionDetailCache.current.clear();
        setTimeline(emptyConversation); setComposer(''); setSessionId(''); setChatDetailOpen(false); setMissionDetailView(null);
        setData(emptyData); setTruncated({}); setHasSynced(false); firstDataShown.current = false; setCachedHostId(''); setSnapshot(null); setStatus('connecting');
        void AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {});
      }
      // Show attention and sessions quickly on first load. If either fails, the full read below
      // reports which collection is unavailable instead of hiding it behind an empty view.
      const first = firstDataShown.current ? null : await Promise.all([
        client.attentionList({ limit }),
        client.sessionsList({ limit }),
      ]).catch((cause: unknown) => { if (isSnapshotChurn(cause)) throw cause; return null; });
      if (first) {
        const [attentionFirst, sessionsFirst] = first;
        if (generation !== cacheGeneration.current) return;
        const initial: Data = {
          ...emptyData,
          attention: items(attentionFirst.value, 'attention').filter(a => a.state === 'open' && a.actions.length > 0 && a.attention_kind !== 'unread-message'),
          sessions: items(sessionsFirst.value, 'session') as SessionView[],
        };
        const incomplete: Array<keyof Data> = ['messages', 'agents', 'missions', 'launches', 'machines', 'devices', 'runtimes', 'work'];
        if (attentionFirst.value.page.has_more) incomplete.push('attention');
        if (sessionsFirst.value.page.has_more) incomplete.push('sessions');
        setData(initial); setTruncated(Object.fromEntries(incomplete.map(key => [key, true])));
        setCaps(capability.value); setSnapshot(attentionFirst.snapshot); setCachedHostId(attentionFirst.snapshot.host_id);
        setHasSynced(true); firstDataShown.current = true; setStatus('online');
        const encoded = encodeProjectionCache(url, capability.value.session_actor, attentionFirst.snapshot.host_id, attentionFirst.snapshot.store_index, initial, Date.now(), incomplete);
        if (encoded) void AsyncStorage.setItem(PROJECTION_CACHE_KEY, encoded).catch(() => {});
      }
      const keys = ['attention', 'messages', 'agents', 'missions', 'launches', 'machines', 'devices', 'runtimes', 'work'] as const;
      const [settled, sessionsResult] = await Promise.all([withConcurrency([
        () => listCollectionPages(options => client.attentionList(options), limit, 10),
        () => listCollectionPages(options => client.messagesList(options), limit),
        () => listCollectionPages(options => client.agentsList(options), limit),
        () => listCollectionPages(options => client.missionsList(options), limit),
        () => listCollectionPages(options => client.launchesList(options), limit),
        () => listCollectionPages(options => client.machinesList(options), limit),
        () => listCollectionPages(options => client.devicesList(options), limit),
        () => listCollectionPages(options => client.runtimesList(options), limit),
        () => listCollectionPages(options => client.workList(options), limit),
      ], 4), listSessionPages(options => client.sessionsList(options), limit).then(value => ({ value }), (reason: unknown) => ({ reason }))]);
      if (generation !== cacheGeneration.current) return;
      // Snapshot churn means the pages disagree; retry the whole read rather than mixing snapshots.
      const churn = settled.find((result): result is PromiseRejectedResult => result.status === 'rejected' && isSnapshotChurn(result.reason));
      if (churn) throw churn.reason;
      if ('reason' in sessionsResult && isSnapshotChurn(sessionsResult.reason)) throw sessionsResult.reason;
      const { values, errors: collectionErrors, firstError } = settleCollections(keys, settled, errorText);
      const errors: Partial<Record<keyof Data, string>> = { ...collectionErrors };
      if (firstError !== undefined) throw firstError;
      if ('reason' in sessionsResult) errors.sessions = errorText(sessionsResult.reason);
      const firstSnapshot = keys.map(key => values[key]?.pages[0]?.snapshot).find(Boolean)!;
      setCaps(capability.value); setSnapshot(firstSnapshot);
      const previous = dataRef.current;
      const read = <K extends keyof Data>(key: K, pick: (collection: CollectionResult) => Data[K]): Data[K] => { const collection = values[key as typeof keys[number]]; return collection ? pick(collection) : previous[key]; };
      const fresh: Data = { attention: read('attention', c => collectionItems(c, 'attention').filter(a => a.state === 'open' && a.actions.length > 0 && a.attention_kind !== 'unread-message')), messages: read('messages', c => collectionItems(c, 'message')), agents: read('agents', c => collectionItems(c, 'agent').filter(agent => (agent as { operational?: { layer?: string } }).operational?.layer !== 'history')), missions: read('missions', c => collectionItems(c, 'mission')), launches: read('launches', c => collectionItems(c, 'launch')), machines: read('machines', c => c.pages.flatMap(page => page.value.items.filter(i => (i as unknown as { kind: string }).kind === 'machine')) as unknown as MachineView[]), devices: read('devices', c => collectionItems(c, 'device')), sessions: 'value' in sessionsResult ? sessionsResult.value : previous.sessions, runtimes: read('runtimes', c => collectionItems(c, 'runtime')), work: read('work', c => collectionItems(c, 'work')) };
      const truncatedKeys: Array<keyof Data> = keys.filter(key => values[key]?.truncated || (!values[key] && truncatedRef.current[key]));
      setData(fresh); setTruncated(Object.fromEntries(truncatedKeys.map(key => [key, true]))); setLoadErrors(errors); setCachedHostId(firstSnapshot.host_id);
      dirtyProjectionTabs.current.clear();
      const now = Date.now(), index = firstSnapshot.store_index, actor = capability.value.session_actor;
      if (cachedActor.current !== actor || cachedIndex.current !== index || now - cacheSavedAt.current > 5 * 60 * 1000) {
        const encoded = encodeProjectionCache(url, actor, firstSnapshot.host_id, index, fresh, now, truncatedKeys);
        if (encoded) { cachedActor.current = actor; cachedIndex.current = index; cacheSavedAt.current = now; void AsyncStorage.setItem(PROJECTION_CACHE_KEY, encoded).catch(() => { cacheSavedAt.current = 0; }); }
      }
      cachedActor.current = actor; cachedIndex.current = index;
      projectionEventCursor.current = capability.value.event_cursor;
      lastNativeRefreshAt.current = Date.now();
      snapshotRetry.current = 0; setHasSynced(true); setStatus('online'); setError('');
    } catch (e) {
      if (generation !== cacheGeneration.current) return;
      if (isSnapshotChurn(e)) {
        const delay = Math.min(2000, 200 * 2 ** Math.min(snapshotRetry.current++, 4));
        setStatus(s => s === 'online' ? s : 'connecting');
        if (foreground.current.active) setTimeout(() => { void refresh(); }, delay);
      } else {
        setStatus('offline');
        setError(e instanceof ClientError && e.status >= 400 && e.status < 500 ? errorText(e) : '');
      }
    } finally { refreshing.current.finish(generation); }
  }, [client, credential, url]);
  const refreshNativeSessions = useCallback(async () => {
    const generation = cacheGeneration.current;
    if (!client || refreshing.current.isCurrent(generation)) return;
    const native = await listSessionPages(options => client.sessionsList({ ...options, native_only: true }), Math.min(caps?.limits.max_page_items ?? 30, 30));
    if (generation !== cacheGeneration.current) return;
    setData(previous => ({ ...previous, sessions: [...previous.sessions.filter(session => !isUnmanaged(session)), ...native] }));
    lastNativeRefreshAt.current = Date.now();
  }, [client, caps?.limits.max_page_items]);
  useEffect(() => {
    const subscription = AppState.addEventListener('change', state => { foreground.current.update(state); setAppActive(state === 'active'); });
    return () => subscription.remove();
  }, []);
  useEffect(() => { void refresh(); }, [refresh]);
  // Nothing follows or polls the gateway in the background; returning to the foreground resyncs once.
  useEffect(() => foreground.current.subscribe(isActive => { if (isActive) void refresh(); }), [refresh]);
  useEffect(() => {
    if (!client || status !== 'online' || !appActive) return;
    let live = true;
    async function followProjectionChanges() {
      while (live) {
        if (!foreground.current.active) { await foreground.current.untilActive(); continue; }
        if (!projectionEventCursor.current) { await new Promise(resolve => setTimeout(resolve, 1000)); continue; }
        try {
          const result = await client!.eventsList({ after: projectionEventCursor.current, limit: 100, wait_ms: 30_000 });
          if (!live) return;
          projectionEventCursor.current = result.value.resume_cursor;
          for (const tab of tabsChangedByProjectionEvents(result.value.items)) dirtyProjectionTabs.current.add(tab);
          if (dirtyProjectionTabs.current.has(active)) {
            const wait = coalescedRefreshDelay(lastEventRefreshAt.current, Date.now(), 10_000);
            if (wait) await new Promise(resolve => setTimeout(resolve, wait));
            if (!live || !foreground.current.active) continue;
            lastEventRefreshAt.current = Date.now();
            await refresh();
          }
          else if ((active === 'Chat' || active === 'Fleet') && Date.now() - lastNativeRefreshAt.current >= 30_000) await refreshNativeSessions();
        } catch {
          if (!live || !foreground.current.active) continue;
          await refresh(); // cursor gaps and lost connections both require a bounded resync
          if (live) await new Promise(resolve => setTimeout(resolve, 1000));
        }
      }
    }
    void followProjectionChanges();
    return () => { live = false; };
  }, [active, appActive, client, refresh, refreshNativeSessions, status]);
  useEffect(() => { if (status === 'online' && dirtyProjectionTabs.current.has(active)) void refresh(); }, [active, refresh, status]);
  useEffect(() => {
    if (status === 'online' && (active === 'Chat' || active === 'Fleet') && Date.now() - lastNativeRefreshAt.current >= 30_000) void refreshNativeSessions();
  }, [active, refreshNativeSessions, status]);
  useEffect(() => { if (status !== 'offline' || !appActive) return; const timer = setInterval(() => { void refresh(); }, 15_000); return () => clearInterval(timer); }, [appActive, refresh, status]);
  useEffect(() => { if (!sessionId && data.sessions.some(s => s.state === 'running')) setSessionId(data.sessions.find(s => s.state === 'running')!.id); }, [data.sessions, sessionId]);
  useEffect(() => {
    if (active !== 'Control' || !selectedMissionId || !client || status !== 'online') return;
    let live = true;
    setMissionDetailView(missionDetailCache.current.get(selectedMissionId) ?? null);
    void client.missionsGet(selectedMissionId).then(result => {
      if (!live || result.value.kind !== 'mission') return;
      rememberBounded(missionDetailCache.current, selectedMissionId, result.value, 12);
      setMissionDetailView(result.value);
    }).catch(() => { /* retain the list projection or cached detail while offline */ });
    return () => { live = false; };
  }, [active, selectedMissionId, selectedMissionRevision, client, status]);
  const timelineSession = data.sessions.find(s => s.id === sessionId) ?? historicalSessions.find(s => s.id === sessionId);
  const timelineUnresolved = timelineSession ? isUnresolved(timelineSession) : false;
  const maxPageItems = caps?.limits.max_page_items;
  useEffect(() => {
    if (!client || !sessionId || timelineUnresolved) { setTimeline(emptyConversation); return; }
    if (status !== 'online' || !appActive || active !== 'Chat' || !chatDetailOpen || terminalId) return;
    const timelineClient = client;
    let live = true;
    let polling = false;
    let lastRevision = '';
    let lastPollAt = 0;
    let eventCursor = capsRef.current?.event_cursor;
    async function poll() {
      if (polling || !live) return;
      polling = true;
      for (let attempt = 0; attempt < 4 && live; attempt++) {
        try {
          const loaded = await loadRecentConversation(options => timelineClient.timelineList(sessionId, options), { pageSize: Math.min(maxPageItems ?? 30, 100), maxPages: 10, want: 100, previous: conversationCache.current.get(sessionId) });
          const revision = `${loaded.hasOlder}|${loaded.entries.map(entry => `${entry.id}:${entry.revision}`).join('|')}`;
          if (live) rememberBounded(conversationCache.current, sessionId, loaded, 24);
          if (live && revision !== lastRevision) {
            lastRevision = revision;
            setTimeline(loaded);
          }
          break;
        } catch (error) {
          if (!isSnapshotChurn(error)) {
            if (live && error instanceof ClientError && error.status >= 400 && error.status < 500) setError(errorText(error));
            break;
          }
          if (attempt < 3) await new Promise(resolve => setTimeout(resolve, 150 * (attempt + 1)));
        }
      }
      polling = false;
      lastPollAt = Date.now();
    }
    async function followVisibleConversation() {
      await poll();
      while (live) {
        if (!foreground.current.active) { await foreground.current.untilActive(); continue; }
        if (!eventCursor) {
          await new Promise(resolve => setTimeout(resolve, 1000));
          eventCursor = capsRef.current?.event_cursor;
          continue;
        }
        try {
          const events = await timelineClient.eventsList({ after: eventCursor, limit: 100, wait_ms: 5_000 });
          eventCursor = events.value.resume_cursor;
          if (events.value.items.some(event => event.resource_ids.includes(sessionId)) || Date.now() - lastPollAt >= 5_000) await poll();
        } catch (error) {
          if (error instanceof ClientError && error.response.code === 'cursor-gap') {
            try { eventCursor = (await timelineClient.capabilities()).value.event_cursor; await poll(); }
            catch { await new Promise(resolve => setTimeout(resolve, 1000)); }
          } else {
            await new Promise(resolve => setTimeout(resolve, 1000));
          }
        }
      }
    }
    void followVisibleConversation();
    return () => { live = false; };
  }, [client, sessionId, status, appActive, maxPageItems, timelineUnresolved, active, chatDetailOpen, terminalId]);
  useEffect(() => {
    if (!client || !terminalId || !chatDetailOpen || active !== 'Chat' || !appActive) return;
    let live = true, polling = false, unavailable = false;
    async function poll() {
      if (!live || polling || unavailable || status !== 'online' || !foreground.current.active) return;
      polling = true;
      try {
        const result = await client!.terminalScreen(terminalId);
        if (live) {
          if (!terminalIncarnation.current) terminalIncarnation.current = result.value.runtime_incarnation;
          setScreen(result.value);
          setTerminalIssue(terminalIncarnation.current === result.value.runtime_incarnation ? '' : 'Terminal restarted; reopen it before sending input.');
        }
      } catch (error) {
        if (live && error instanceof ClientError && error.status >= 400 && error.status < 500) { unavailable = true; setTerminalIssue(errorText(error)); }
      } finally { polling = false; }
    }
    void poll();
    const timer = setInterval(() => { void poll(); }, 1500);
    return () => { live = false; clearInterval(timer); };
  }, [client, terminalId, chatDetailOpen, active, appActive, status]);

  async function review(id: string) { if (!client || status !== 'online') return; try { const result = await client.launchVariantsList(id, { limit: Math.min(caps?.limits.max_page_items ?? 30, 30) }); setReviewLaunch(id); setVariants(items(result.value, 'launch-variant')); setError(''); } catch (e) { setError(errorText(e)); } }
  async function preview(launch: Launch, variant: LaunchVariant) { if (!client) return; await runAction(() => { const id = actionId(); return client.launchPreview({ id, idempotency_key: id, fence: fence({ [launch.id]: launch.revision, [variant.id]: variant.revision }), parameters: { launch_id: launch.id, variant_id: variant.id } }); }); void review(launch.id); }
  async function approve(launch: Launch, variant: LaunchVariant) { if (!client || !variant.preview_token) return; await runAction(() => { const id = actionId(); return client.launchApprove({ id, idempotency_key: id, fence: { ...fence({ [launch.id]: launch.revision, [variant.id]: variant.revision }), preview_token: variant.preview_token! }, parameters: { launch_id: launch.id, variant_id: variant.id } }); }); setVariants([]); }
  function showTerminal(id: string) { if (!client || status !== 'online') return; terminalIncarnation.current = ''; setTerminalDraft(''); setTerminalActionNotice(''); setScreen(null); setTerminalIssue(''); setTerminalId(id); setError(''); }
  async function sendTerminalInput(mode: 'line' | 'key', value: string) {
    if (!client || !terminalId || status !== 'online' || busy || terminalSending.current || !value || !terminalIncarnation.current) return;
    const incarnation = terminalIncarnation.current;
    terminalSending.current = true;
    setBusy(true);
    try {
      await withFreshTerminalFence(client, terminalId, incarnation, fence => {
        const id = actionId();
        return client.terminalInput({ id, idempotency_key: id, fence, parameters: { terminal_id: terminalId, mode, value } });
      });
      if (mode === 'line') setTerminalDraft('');
      const latest = await client.terminalScreen(terminalId);
      if (latest.value.runtime_incarnation !== incarnation) throw new Error('Terminal restarted; reopen it before sending input.');
      setScreen(latest.value); setTerminalIssue(''); setTerminalActionNotice(''); setError('');
    } catch (cause) { setTerminalActionNotice(`Input was not confirmed. Inspect the screen before retrying: ${errorText(cause)}`); }
    finally { terminalSending.current = false; setBusy(false); }
  }
  async function clearCachedProjection() { cacheGeneration.current++; cachedActor.current = ''; cachedIndex.current = -1; cacheSavedAt.current = 0; projectionEventCursor.current = null; dirtyProjectionTabs.current.clear(); lastNativeRefreshAt.current = 0; firstDataShown.current = false; conversationCache.current.clear(); draftCache.current.clear(); chatScrollCache.current.clear(); missionDetailCache.current.clear(); setMissionDetailView(null); setData(emptyData); setTruncated({}); setHasSynced(false); setCachedHostId(''); setSnapshot(null); setTimeline(emptyConversation); await AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {}); }
  async function saveUrl() { const normalized = normalizeGatewayUrl(urlDraft); if (!normalized) { setError('Enter the paired gateway HTTPS URL, or http:// with a Tailscale address (100.x), a .local name, or a private LAN address (10.x, 172.16-31.x, 192.168.x).'); return; } if (normalized !== url) await clearCachedProjection(); await AsyncStorage.setItem(URL_KEY, normalized); setUrl(normalized); setError(''); }
  async function pair() { if (!client || !pairingId.trim() || !pairingCode.trim()) return; setBusy(true); try {
    const publicKey = Array.from(Crypto.getRandomBytes(32), b => b.toString(16).padStart(2, '0')).join('');
    const result = await client.completePairing(pairingId.trim(), { api_version: API_VERSION, code: pairingCode.trim(), device_public_key: publicKey });
    await clearCachedProjection();
    await SecureStore.setItemAsync(CREDENTIAL_KEY, result.value.credential, { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY });
    setCredential(result.value.credential); setPairingCode(''); setPairingId(''); setPairingIssue(''); setError('');
  } catch (e) { setError(errorText(e)); } finally { setBusy(false); } }
  async function runAction(action: () => Promise<unknown>) { if (status !== 'online') { setError('Reconnect before sending an action.'); return; } setBusy(true); try { await action(); setError(''); await refresh(); } catch (e) { setError(errorText(e)); } finally { setBusy(false); } }
  function fence(revisions: Record<string, string> = {}) { if (!snapshot) throw new Error('Refresh before acting.'); return { snapshot_id: snapshot.id, subject_revisions: revisions }; }
  function actionId() { return `action/ios-${Crypto.randomUUID()}`; }
  async function send() { const session = data.sessions.find(s => s.id === sessionId); if (!client || !session || isUnmanaged(session) || session.state !== 'running' || !composer.trim()) return; const content = composer.trim(); await runAction(async () => { const id = actionId(); await client.messageSend({ id, idempotency_key: id, fence: fence(), parameters: { content, session_id: session.id, to: session.owner_id } }); draftCache.current.delete(sessionId); setComposer(''); }); }
  async function createLaunch() { if (!client || !title.trim() || !request.trim() || !workspace.trim()) return; await runAction(async () => { const id = actionId(); await client.launchCreate({ id, idempotency_key: id, fence: fence(), parameters: { title: title.trim(), request: request.trim(), target: { type: 'new-mission', mission_id: `mission/ios-${Crypto.randomUUID()}`, workspace: workspace.trim() }, provider, ...(model.trim() ? { model: model.trim() } : {}), ...(effort.trim() ? { effort: effort.trim() } : {}) } }); setTitle(''); setRequest(''); }); }
  async function reviseLaunch(launch: Launch) { if (!client || !feedback.trim()) return; await runAction(async () => { const id = actionId(); await client.launchRevise({ id, idempotency_key: id, fence: fence({ [launch.id]: launch.revision }), parameters: { launch_id: launch.id, feedback: feedback.trim() } }); setFeedback(''); }); }
  async function resolve(item: Attention) { if (!client) return; await runAction(() => { const id = actionId(); return client.attentionResolve({ id, idempotency_key: id, fence: fence({ [item.id]: item.revision }), parameters: { attention_id: item.id, outcome: 'resolved' } }); }); }
  async function forget() { await SecureStore.deleteItemAsync(CREDENTIAL_KEY); await clearCachedProjection(); setCredential(null); setCaps(null); setPairingIssue(''); setHistoricalSessions([]); setShowHistory(false); setSessionId(''); setTerminalId(''); terminalIncarnation.current = ''; setTerminalDraft(''); setTerminalActionNotice(''); setScreen(null); setTerminalIssue(''); setStatus('setup'); }
  async function openHistory() { if (!client || !caps) return; setHistoryBusy(true); try { setHistoricalSessions((await listSessionPages(options => client.sessionsList(options), Math.min(caps.limits.max_page_items, 30), true)).filter(s => ['completed', 'failed', 'cancelled'].includes(s.state))); setShowHistory(true); setError(''); } catch (e) { if (!isSnapshotChurn(e)) setError(errorText(e)); } finally { setHistoryBusy(false); } }
  function move(tab: Tab, direction: -1 | 1) { const index = order.indexOf(tab), next = index + direction; if (next < 0 || next >= order.length) return; const updated = [...order]; [updated[index], updated[next]] = [updated[next], updated[index]]; setOrder(updated); void AsyncStorage.setItem(ORDER_KEY, JSON.stringify(updated)); }
  const selectedSession = [...data.sessions, ...historicalSessions].find(s => s.id === sessionId);
  const canControlTerminal = caps?.capabilities.some(capability => capability.id === 'terminal.input' && capability.state === 'granted') ?? false;
  const selectedAgent = selectedSession ? data.agents?.find(agent => agent.id === selectedSession.owner_id) : undefined;
  const currentSessions = data.sessions.filter(s => s.state === 'running').sort((a, b) => Number(isUnmanaged(b)) - Number(isUnmanaged(a)));
  const knownHostId = snapshot?.host_id ?? cachedHostId;
  const gatewayMachineId = knownHostId ? `machine/${knownHostId.replace(/^host\//, '')}` : '';
  const sourceHost = data.machines.find(m => m.id === gatewayMachineId)?.name ?? knownHostId?.replace(/^host\//, '') ?? 'connected gateway host';
  const undeclaredSessions = currentSessions.filter(isUnmanaged);
  const managedSessions = currentSessions.filter(s => !isUnmanaged(s));
  const declaredAgentRows = agentTree(data.agents);
  const representedSessions = new Set(declaredAgentRows.flatMap(({ agent }) => managedSessions.filter(s => s.id === agent.current_session_id || s.owner_id === agent.id).map(s => s.id)));
  const unmatchedDeclaredSessions = managedSessions.filter(s => !representedSessions.has(s.id));
  const sessionMessages = selectedSession ? sessionMessagesFor(data.messages, selectedSession) : [];
  const conversationEntries = timeline.entries;
  const now = Date.now();
  const missionNames = missionLabels(data.missions);
  const unhealthyAgents = data.agents.filter(agent => !agentHealth(agent).healthy);
  const attentionState = attentionHeadline({ count: data.attention.length, loaded: hasSynced, error: loadErrors.attention ?? (status === 'offline' ? error || 'the gateway is unreachable' : undefined) });
  const otherLoadErrors = Object.entries(loadErrors).filter(([key]) => key !== 'attention');
  function selectSession(id: string) { if (sessionId && chatDetailOpen) rememberBounded(chatScrollCache.current, sessionId, currentScrollY.current, 24); setSessionId(id); setTimeline(conversationCache.current.get(id) ?? emptyConversation); setComposer(draftCache.current.get(id) ?? ''); setTerminalId(''); terminalIncarnation.current = ''; setTerminalDraft(''); setTerminalActionNotice(''); setScreen(null); setTerminalIssue(''); setChatDetailOpen(true); }
  const sessionChoice = (s: SessionView) => <Pressable key={s.id} onPress={() => selectSession(s.id)} style={[styles.choice, sessionId === s.id && styles.selected]}><Text style={styles.cardTitle}>{sessionLabel(s, sourceHost)}</Text><Text style={styles.small}>{sessionDetail(s)}</Text></Pressable>;
  const visibleMissions = data.missions.filter(m => showSystemMissions || !m.id.startsWith('mission/__st3/'));
  const selectedMission = missionDetailView?.id === selectedMissionId ? missionDetailView : visibleMissions.find(m => m.id === selectedMissionId);

  return <SafeAreaView style={styles.page}>
    <View style={styles.header}><Text style={styles.brand}>Smalltalk</Text><Text style={[styles.status, status === 'online' && styles.good]}>{status === 'online' ? 'Connected' : status === 'connecting' ? hasSynced ? 'Updating · showing last data' : 'Connecting…' : status === 'offline' ? offlinePresentation(hasSynced).title : 'Pair this device'}</Text></View>
    {error ? <Pressable onPress={() => setError('')} style={styles.error}><Text style={styles.errorText}>{error}</Text></Pressable> : null}
    {pairingIssue ? <Pressable onPress={() => setPairingIssue('')} style={styles.error}><Text style={styles.errorText}>{pairingIssue}</Text></Pressable> : null}
    {otherLoadErrors.length && status === 'online' ? <View style={styles.error}><Text style={styles.errorText}>Not loaded: {otherLoadErrors.map(([key, message]) => `${key} (${message})`).join(' · ')}</Text></View> : null}
    {busy ? <ActivityIndicator color="#67d6c5" /> : null}
    <ScrollView ref={scrollView} style={styles.content} contentContainerStyle={styles.scroll} keyboardShouldPersistTaps="handled" scrollEventThrottle={100} onScroll={event => { currentScrollY.current = event.nativeEvent.contentOffset.y; if (active === 'Chat' && chatDetailOpen && sessionId) rememberBounded(chatScrollCache.current, sessionId, currentScrollY.current, 24); }}>
      {!url || !credential ? <><Text style={styles.title}>Connect to Smalltalk</Text><Text style={styles.muted}>Use the paired-only gateway: its HTTPS URL; http:// with the host's Tailscale address (100.x.y.z), which Tailscale encrypts; or http:// with its .local name or private LAN address, which is not encrypted. Begin pairing on a trusted st3 machine, then enter its short-lived ID and code.</Text>
        <TextInput style={styles.input} autoCapitalize="none" autoCorrect={false} keyboardType="url" placeholder="https://gateway, http://100.x.y.z:port, or http://host.local:port" placeholderTextColor="#8195a2" value={urlDraft} onChangeText={setUrlDraft} />{gatewayTransport(urlDraft) === 'lan' ? <Text style={styles.warning}>{LAN_HTTP_WARNING}</Text> : null}<Button label="Save gateway" onPress={() => void saveUrl()} />
        {url ? <><TextInput style={styles.input} autoCapitalize="none" placeholder="Pairing ID" placeholderTextColor="#8195a2" value={pairingId} onChangeText={setPairingId} /><TextInput style={styles.input} autoCapitalize="none" placeholder="Pairing code" placeholderTextColor="#8195a2" value={pairingCode} onChangeText={setPairingCode} /><Button label="Pair device" disabled={busy} onPress={() => void pair()} /></> : null}</> : !hasSynced && status !== 'online' ? <>
        <Text style={styles.title}>{status === 'offline' ? 'Offline' : 'Connecting…'}</Text>
        <Text style={styles.muted}>{status === 'offline' ? offlinePresentation(false).detail : 'Loading your workspace for the first time.'}</Text>
        {status === 'offline' ? <Button label="Reconnect" onPress={() => void refresh()} /> : null}
      </> : <>
        {status === 'offline' ? <Button label="Reconnect" onPress={() => void refresh()} /> : null}
        {active === 'Now' ? <>
          <Text style={styles.title}>Needs your attention</Text>
          <Text style={attentionState.warning ? styles.warning : styles.muted}>{attentionState.text}</Text>
          {truncated.attention ? <Text style={styles.warning}>More attention items exist beyond this view. Open the full inbox in the CLI to see them all.</Text> : null}
          {data.attention.map(a => {
            const expanded = expandedAttentionId === a.id;
            return <Card key={a.id} title={a.title}>
              <Text style={styles.small}>{a.priority} · {attentionKindLabel(a.attention_kind)} · requested {ago(a.requested_at, now)} ago</Text>
              <Text style={styles.muted} numberOfLines={expanded ? undefined : 3}>{a.detail}</Text>
              {expanded ? <Text style={styles.small}>Source: {a.source_id}</Text> : null}
              <Pressable accessibilityRole="button" accessibilityState={{ expanded }} onPress={() => setExpandedAttentionId(expanded ? null : a.id)} style={styles.detailToggle}>
                <Text style={styles.detailToggleText}>{expanded ? 'Hide details' : 'Show details'}</Text>
              </Pressable>
              {a.actions.some(action => action !== 'attention.resolve') ? <Text style={styles.small}>In the CLI: {a.actions.filter(action => action !== 'attention.resolve').map(attentionActionLabel).join(', ')}</Text> : null}
              {a.actions.includes('attention.resolve') ? <Button label={attentionActionLabel('attention.resolve')} disabled={busy} onPress={() => Alert.alert('Resolve attention?', a.title, [{ text: 'Cancel' }, { text: 'Resolve', onPress: () => void resolve(a) }])} /> : null}
            </Card>;
          })}
          <Button label="Refresh" onPress={() => void refresh()} />
        </> : null}
        {active === 'Chat' ? chatDetailOpen && selectedSession ? <>
          <Button label="← Agents" onPress={() => { setChatDetailOpen(false); setTerminalId(''); terminalIncarnation.current = ''; setTerminalDraft(''); setTerminalActionNotice(''); setScreen(null); setTerminalIssue(''); }} />
          <Text style={styles.section}>{selectedAgent ? agentLabel(selectedAgent) : sessionLabel(selectedSession, sourceHost)}</Text>
          {selectedAgent ? <Text style={agentHealth(selectedAgent).healthy ? styles.muted : styles.warning}>{agentHeaderDetail(selectedAgent, now)}</Text> : null}
          <Text style={styles.muted}>{sessionDetail(selectedSession)}</Text>
          {terminalId ? <>
            <Button label="← Conversation" onPress={() => { terminalIncarnation.current = ''; setTerminalDraft(''); setTerminalActionNotice(''); setScreen(null); setTerminalId(''); setTerminalIssue(''); }} />
            <Text style={styles.section}>{canControlTerminal ? 'Terminal · live controls' : 'Terminal · read-only'}</Text>
            {terminalIssue ? <Text style={styles.warning}>{terminalIssue}</Text> : null}
            {terminalActionNotice ? <Text style={styles.warning}>{terminalActionNotice}</Text> : null}
            {screen && canControlTerminal ? <>
              <TextInput style={styles.input} autoCapitalize="none" autoCorrect={false} placeholder="Type a terminal line" placeholderTextColor="#8195a2" value={terminalDraft} onChangeText={setTerminalDraft} />
              <Button label="Send line" disabled={busy || status !== 'online' || !!terminalIssue || !terminalDraft.length} onPress={() => void sendTerminalInput('line', terminalDraft)} />
              <View style={styles.row}>
                {(['return', 'tab', 'escape', 'Up', 'Down'] as const).map(key => <Button key={key} label={key === 'return' ? 'Enter' : key === 'escape' ? 'Esc' : key} disabled={busy || status !== 'online' || !!terminalIssue} onPress={() => void sendTerminalInput('key', key)} />)}
                <Button label="Ctrl-C" disabled={busy || status !== 'online' || !!terminalIssue} onPress={() => Alert.alert('Interrupt terminal process?', 'Send Ctrl-C to this terminal.', [{ text: 'Cancel' }, { text: 'Interrupt', onPress: () => void sendTerminalInput('key', 'C-c') }])} />
              </View>
              <Text style={styles.muted}>Inputs require a live paired connection and are fenced to this terminal incarnation.</Text>
            </> : null}
            <Card title="Screen" detail={screen ? screen.lines.map(line => line.text).join('\n') : terminalIssue || (status === 'online' ? 'Loading terminal screen…' : 'Offline; no terminal screen is cached.')} />
            {screen && status !== 'online' ? <Text style={styles.muted}>Offline · showing the last terminal frame.</Text> : null}
          </> : <>
            {!isUnmanaged(selectedSession) && selectedSession.state === 'running' ? data.runtimes.filter(r => r.terminal_id && r.owner_id === selectedSession.owner_id).map(r => <Button key={r.id} label="View terminal" disabled={status !== 'online'} onPress={() => void showTerminal(r.terminal_id!)} />) : null}
            <Text style={styles.section}>Conversation</Text>
            {truncated.messages ? <Text style={styles.warning}>The message fallback is partial; open this session in the CLI for complete history.</Text> : null}
            {isUnresolved(selectedSession) ? <Text style={styles.muted}>This process has no exact native session history.</Text> : null}
            {conversationRows(conversationEntries, timeline.hasOlder).map(row => {
              if (row.kind === 'older') return <Text key="older" style={styles.muted}>Older history is not shown here. Open this session in the CLI for the full transcript.</Text>;
              const shown = pingPresentation(timelineText(row.entry.body) ?? '');
              return <Card key={row.entry.id} title={shown.from ?? (row.entry.role === 'assistant' ? 'Agent' : row.entry.role === 'user' ? 'You' : row.entry.role)} detail={shown.text} />;
            })}
            {!conversationEntries.length && sessionMessages.length ? <><Text style={styles.section}>Recent messages</Text>{sessionMessages.map(m => <Card key={m.id} title={`${m.from === selectedSession.owner_id ? 'Agent' : m.from} → ${m.to === selectedSession.owner_id ? 'agent' : m.to}${m.title ? ` · ${m.title}` : ''}`} detail={pingPresentation(m.content).text} />)}</> : null}
            {!conversationEntries.length && !sessionMessages.length && !isUnresolved(selectedSession) ? <Text style={styles.muted}>No conversation in the recent timeline.</Text> : null}
            {!isUnmanaged(selectedSession) && selectedSession.state === 'running' ? <><TextInput style={[styles.input, styles.composer]} multiline placeholder="Message this session" placeholderTextColor="#8195a2" value={composer} onChangeText={text => { if (text) rememberBounded(draftCache.current, sessionId, text, 24); else draftCache.current.delete(sessionId); setComposer(text); }} /><Button label="Send" disabled={busy || status !== 'online' || !composer.trim()} onPress={() => void send()} /></> : null}
          </>}
        </> : <>
          <Text style={styles.title}>Chat</Text><Text style={styles.muted}>Declared agents across the fleet; undeclared sessions discovered on this gateway.</Text>
          <Text style={styles.section}>Undeclared on {sourceHost}</Text>{undeclaredSessions.map(sessionChoice)}{!undeclaredSessions.length ? <Text style={styles.muted}>None discovered on this machine.</Text> : null}
          {unhealthyAgents.length ? <Text style={styles.warning}>Unhealthy: {unhealthyAgents.map(agent => `${agentLabel(agent)} (${agentHealth(agent).label})`).join(' · ')}</Text> : null}
          <Text style={styles.section}>Declared agents</Text>{truncated.agents ? <Text style={styles.warning}>More agents exist beyond this view.</Text> : null}
          {declaredAgentRows.map(({ agent, depth }) => {
            const session = managedSessions.find(s => s.id === agent.current_session_id) ?? managedSessions.find(s => s.owner_id === agent.id);
            return <Pressable key={agent.id} disabled={!session} onPress={() => { if (session) selectSession(session.id); }} style={[styles.choice, { marginLeft: Math.min(depth, 4) * 14 }, session?.id === sessionId && styles.selected]}><Text style={styles.cardTitle}>{depth ? '↳ ' : ''}{agentLabel(agent)}</Text><Text style={agentHealth(agent).healthy ? styles.small : styles.warning}>{agentHealth(agent).label} · observed {ago(agent.updated_at, now)} ago</Text><Text style={styles.small}>{agent.owner_run_id ? agent.owner_run_id.replace(/^mission-run\//, '') : 'no owner run'}{session ? ` · ${session.state} session` : ' · no current session'}</Text>{agent.current_work_ids?.[0] ? <Text style={styles.small}>Current: {agent.current_work_ids[0]}</Text> : null}{queuedWorkSummary(agent, data.work, now) ? <Text style={styles.small}>{queuedWorkSummary(agent, data.work, now)}</Text> : null}</Pressable>;
          })}
          {unmatchedDeclaredSessions.length ? <><Text style={styles.section}>Other declared sessions</Text>{unmatchedDeclaredSessions.map(sessionChoice)}</> : null}
          {!declaredAgentRows.length && !managedSessions.length ? <Text style={styles.muted}>No declared agents are visible.</Text> : null}
          <Text style={styles.section}>Past sessions</Text><Button label={showHistory ? 'Refresh past sessions' : 'Show past sessions'} disabled={historyBusy || status !== 'online'} onPress={() => void openHistory()} />{showHistory ? historicalSessions.map(sessionChoice) : null}
        </> : null}
        {active === 'Control' ? selectedMission ? <>
          <Button label="← Missions" onPress={() => setSelectedMissionId('')} />
          <Text style={styles.title}>{missionNames.get(selectedMission.id) ?? selectedMission.title}</Text>
          <Text style={styles.muted}>{missionGroup(selectedMission, data.work)} · {selectedMission.id}</Text>
          <Card title="Work tree" detail={`${selectedMission.runs.length} runs · ${selectedMission.visualization?.nodes.filter(node => node.kind === 'step').length ?? 'unknown'} planned steps`}>
            {data.work.filter(w => selectedMission.runs.includes(w.mission_run_id)).sort((a, b) => a.path.localeCompare(b.path)).map(w => <View key={w.id} style={{ marginLeft: Math.min(3, w.path.split('/').length - 1) * 14, marginTop: 10 }}><Text style={styles.cardTitle}>↳ {w.path.split('/').pop()} · {w.state}</Text>{w.blocked_reason ? <Text style={styles.warning}>Blocked: {w.blocked_reason}</Text> : null}{w.goals[0] ? <Text style={styles.muted}>Goal: {w.goals[0]}</Text> : null}{w.claimant ? <Text style={styles.small}>Agent: {w.claimant}</Text> : null}</View>)}
            {selectedMission.visualization?.groups.filter(g => g.kind === 'nested-mission').map(g => <Text key={g.id} style={styles.muted}>↳ Nested mission: {g.members.join(', ')}</Text>)}
            {!data.work.some(w => selectedMission.runs.includes(w.mission_run_id)) ? <Text style={styles.muted}>No current work is visible in this snapshot.</Text> : null}
          </Card>
          {truncated.work ? <Text style={styles.warning}>More work exists beyond this view; use the CLI for the complete tree.</Text> : null}
        </> : <>
          <Text style={styles.title}>Control</Text>
          <Text style={styles.muted}>Missions grouped by what needs action. Select one to see its work tree and blockers.</Text>
          {truncated.missions || truncated.work ? <Text style={styles.warning}>This view is partial. Use the CLI for complete mission and work lists.</Text> : null}
          {missionGroups.map(group => {
            const missions = visibleMissions.filter(m => missionGroup(m, data.work) === group);
            return missions.length ? <View key={group}><Text style={styles.section}>{group} · {missions.length}</Text>{missions.map(m => <Pressable key={m.id} onPress={() => setSelectedMissionId(m.id)} style={[styles.choice, selectedMissionId === m.id && styles.selected]}><Text style={styles.cardTitle}>{missionNames.get(m.id) ?? m.title}</Text><Text style={styles.small}>{missionDetail(m, data.work)}</Text></Pressable>)}</View> : null;
          })}
          <Button label={showSystemMissions ? 'Hide system missions' : 'Show system missions'} onPress={() => setShowSystemMissions(!showSystemMissions)} />
          <Text style={styles.section}>Plan a mission</Text><Button label={showPlanner ? 'Hide planner' : 'New mission'} onPress={() => setShowPlanner(!showPlanner)} />
          {showPlanner ? <>
          <Text style={styles.muted}>Start a configurable planner. Review and approval stay in st3.</Text>
          <TextInput style={styles.input} placeholder="Title" placeholderTextColor="#8195a2" value={title} onChangeText={setTitle} />
          <TextInput style={[styles.input, styles.composer]} multiline placeholder="What should be done?" placeholderTextColor="#8195a2" value={request} onChangeText={setRequest} />
          <TextInput style={styles.input} autoCapitalize="none" placeholder="Workspace on target machine" placeholderTextColor="#8195a2" value={workspace} onChangeText={setWorkspace} />
          <Text style={styles.small}>Planner</Text>
          <View style={styles.row}>{(['codex', 'claude', 'pi', 'omp', 'opencode'] as const).map(p => <Pressable key={p} onPress={() => setProvider(p)} style={[styles.chip, provider === p && styles.selected]}><Text style={styles.small}>{p}</Text></Pressable>)}</View>
          <TextInput style={styles.input} placeholder="Model (optional)" placeholderTextColor="#8195a2" value={model} onChangeText={setModel} />
          <TextInput style={styles.input} placeholder="Effort (optional)" placeholderTextColor="#8195a2" value={effort} onChangeText={setEffort} />
          <Button label="Create launch" disabled={busy || !title.trim() || !request.trim() || !workspace.trim()} onPress={() => void createLaunch()} />
          </> : null}
          <Text style={styles.section}>Launches</Text>{truncated.launches ? <Text style={styles.warning}>More launches exist beyond this view. Use the CLI for the complete list.</Text> : null}
          {data.launches.map(l => <Card key={l.id} title={l.title} detail={`${l.phase} · ${l.planner_config.provider} · ${l.id}`}><Text style={styles.muted}>{l.request}</Text><Button label="Review variants" onPress={() => void review(l.id)} />{reviewLaunch === l.id ? variants.map(v => <Card key={v.id} title={`Variant ${v.ordinal} · ${v.status}`} detail={v.diagnostics.map(d => `${d.severity}: ${d.message}`).join(' · ') || 'No diagnostics'}><Button label="Preview" disabled={busy} onPress={() => void preview(l, v)} />{v.preview_token ? <Button label="Approve" disabled={busy} onPress={() => Alert.alert('Approve launch variant?', l.title, [{ text: 'Cancel' }, { text: 'Approve', onPress: () => void approve(l, v) }])} /> : null}</Card>) : null}{l.phase === 'authoring' || l.phase === 'review' ? <><TextInput style={styles.input} placeholder="Planner feedback" placeholderTextColor="#8195a2" value={feedback} onChangeText={setFeedback} /><Button label="Revise" disabled={busy || !feedback.trim()} onPress={() => void reviseLaunch(l)} /></> : null}</Card>)}
        </> : null}
        {active === 'Fleet' ? <>
          <Text style={styles.title}>Fleet</Text>{truncated.machines || truncated.runtimes || truncated.devices ? <Text style={styles.warning}>This fleet view is partial. Use the CLI for the complete machine, runtime, and device lists.</Text> : null}
          <Text style={styles.section}>Agent work</Text>{unhealthyAgents.map(agent => <Text key={`unhealthy-${agent.id}`} style={styles.warning}>{agentLabel(agent)} · {agentHealth(agent).label} · observed {ago(agent.updated_at, now)} ago</Text>)}{data.agents.filter(agent => (agent.active_work_count ?? 0) > 0 || (agent.queued_work_count ?? 0) > 0).map(agent => <Text key={agent.id} style={styles.small}>{agentLabel(agent)} · {agent.active_work_count ?? 0} active{queuedWorkSummary(agent, data.work, now) ? ` · ${queuedWorkSummary(agent, data.work, now)}` : ''}</Text>)}
          {data.machines.map(m => <Card key={m.id} title={m.name} detail={`${m.state} · ${m.occupancy.running_runtimes} runtimes · ${m.capacity.state}`}>
            <Text style={styles.small}>{m.transports.map(t => `${t.protocol}: ${t.status}`).join(' · ')}</Text>
            {m.id === gatewayMachineId ? <><Text style={styles.section}>Undeclared sessions</Text>{undeclaredSessions.map(s => <Text key={s.id} style={styles.muted}>{isUnresolved(s) ? 'Unresolved running process' : 'Exact native session'} · {s.driver ?? 'native harness'}{s.process ? ` · PID ${s.process.pid}` : ''}{s.title ? ` · ${s.title}` : ''}</Text>)}{!undeclaredSessions.length ? <Text style={styles.muted}>None discovered on this machine.</Text> : null}</> : null}
          </Card>)}
          {!data.machines.some(m => m.id === gatewayMachineId) ? <Card title={sourceHost} detail="Connected gateway machine"><Text style={styles.section}>Undeclared sessions</Text>{undeclaredSessions.map(s => <Text key={s.id} style={styles.muted}>{isUnresolved(s) ? 'Unresolved running process' : 'Exact native session'} · {s.driver ?? 'native harness'}{s.process ? ` · PID ${s.process.pid}` : ''}</Text>)}{!undeclaredSessions.length ? <Text style={styles.muted}>None discovered on this machine.</Text> : null}</Card> : null}
          <Text style={styles.muted}>Discovery covers the connected gateway machine only.</Text>
          <Text style={styles.section}>You & devices</Text>
          <Card title="This connection" detail={caps ? `${caps.session_actor} · ${caps.transport}` : 'Reconnecting'}><Text style={styles.small}>{url}</Text>{gatewayTransport(url) === 'lan' ? <Text style={styles.warning}>{LAN_HTTP_WARNING}</Text> : null}<Button label="Forget local credential" onPress={() => Alert.alert('Forget this device?', 'You will need to pair again.', [{ text: 'Cancel' }, { text: 'Forget', onPress: () => void forget() }])} /></Card>
          {data.devices.map(d => <Card key={d.id} title={deviceTitle(d, caps?.session_actor)} detail={deviceDetail(d, now)}><Text style={styles.small}>{d.scopes.join(', ')}</Text></Card>)}
          <Text style={styles.section}>Tab order</Text>
          {order.map(t => <View key={t} style={styles.orderRow}><Text style={styles.cardTitle}>{t}</Text><Button label="↑" onPress={() => move(t, -1)} /><Button label="↓" onPress={() => move(t, 1)} /></View>)}
        </> : null}
      </>}
    </ScrollView><View accessibilityRole="tablist" style={styles.tabs}>{order.map(t => <Pressable key={t} accessibilityRole="tab" accessibilityState={{ selected: active === t }} onPress={() => setActive(t)} style={[styles.tab, active === t && styles.activeTab]}><Text style={[styles.tabText, active === t && styles.activeTabText]}>{t}</Text></Pressable>)}</View>
  </SafeAreaView>;
}
const styles = StyleSheet.create({ page: { flex: 1, backgroundColor: '#101923' }, header: { paddingHorizontal: 22, paddingTop: 16, paddingBottom: 14, borderBottomColor: '#344651', borderBottomWidth: 1 }, brand: { color: '#f3f7fa', fontSize: 24, fontWeight: '700' }, status: { color: '#f0ad69', marginTop: 4 }, good: { color: '#67d6c5' }, content: { flex: 1 }, scroll: { padding: 20, paddingBottom: 48 }, title: { color: '#f3f7fa', fontSize: 27, fontWeight: '700', marginBottom: 10 }, section: { color: '#67d6c5', fontSize: 20, fontWeight: '700', marginTop: 25, marginBottom: 10 }, muted: { color: '#b8c7d0', fontSize: 14, lineHeight: 21, marginTop: 4 }, warning: { color: '#f0c77c', fontSize: 13, lineHeight: 19, marginTop: 8, marginBottom: 6 }, small: { color: '#a9bac5', fontSize: 12, lineHeight: 18 }, card: { backgroundColor: '#1b2b36', borderRadius: 12, padding: 15, marginTop: 10 }, cardTitle: { color: '#f3f7fa', fontSize: 16, fontWeight: '600' }, detailToggle: { alignSelf: 'flex-start', paddingVertical: 8, marginTop: 3 }, detailToggleText: { color: '#67d6c5', fontSize: 14, fontWeight: '600' }, input: { borderWidth: 1, borderColor: '#49606b', borderRadius: 10, color: '#f3f7fa', padding: 12, marginTop: 11, fontSize: 15 }, composer: { minHeight: 86, textAlignVertical: 'top' }, button: { backgroundColor: '#176d69', borderRadius: 9, paddingVertical: 10, paddingHorizontal: 13, alignSelf: 'flex-start', marginTop: 10 }, buttonText: { color: '#fff', fontWeight: '700', fontSize: 14 }, disabled: { opacity: 0.45 }, choice: { borderColor: '#49606b', borderWidth: 1, borderRadius: 10, padding: 10, marginTop: 8 }, selected: { borderColor: '#67d6c5', backgroundColor: '#214144' }, row: { flexDirection: 'row', flexWrap: 'wrap', gap: 5 }, chip: { borderRadius: 8, borderWidth: 1, borderColor: '#49606b', padding: 7, marginTop: 7 }, error: { backgroundColor: '#633b3b', padding: 10 }, errorText: { color: '#fff3ed' }, orderRow: { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingVertical: 5 }, tabs: { flexDirection: 'row', borderTopWidth: 1, borderTopColor: '#344651' }, tab: { flex: 1, alignItems: 'center', paddingVertical: 16 }, activeTab: { borderTopWidth: 3, borderTopColor: '#67d6c5' }, tabText: { color: '#a9bac5', fontSize: 13, fontWeight: '600' }, activeTabText: { color: '#f3f7fa' } });

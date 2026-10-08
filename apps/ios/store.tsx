import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { AppState } from 'react-native';
import AsyncStorage from '@react-native-async-storage/async-storage';
import * as SecureStore from 'expo-secure-store';
import * as Crypto from 'expo-crypto';
import { API_VERSION, ClientError, St3Client, isTransient, notApplied, plainError, retryTransient, type Attention, type AttachmentInput, type Capabilities, type ConversationSearch, type Glass, type Launch, type LaunchVariant, type Mission, type Resource, type Snapshot, type TimelineEntry } from '../../clients/typescript/st3-client';
import { keepClosed, personAnswer, clientName, isSnapshotChurn, listSessionPages, OLDER_PAGE, readOlder, type Conversation, type Older, type SessionView, base64url, messageSubject, signatureParameter, signatureRefusal, signedBytes, type DeviceKey, type Unsigned } from '@smalltalk/st3-views';
import app from './app.json';
import { canVerifyPairing, createDeviceKey, removeDeviceKey, signWithDeviceKey, verifyGrantSignature } from './modules/st-device-key';
import { REPAIR_WARNING, validatePairingTrust, verifyPairing } from './pairingProof';
import { emptyData, encodeProjectionCache, hydrateProjectionForPairedDevice, PROJECTION_CACHE_KEY, type Data } from './projectionCache';
import { listCollectionPages, mergedRows } from './collectionPages';
import { rememberBounded } from './boundedCache';
import { wantsScreenSequence, withFreshTerminalFence, type TerminalFence } from './terminalControls';
import { Feed } from './feed';
import { ForegroundGate } from './foreground';
import type { FabricProfile } from './fabricProof';
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
// The enrolled signing key's public half, the person it signs as and its grant chain (the private
// half stays in the native module).
const SIGNING_KEY = 'st3.device.signing';
// One Keychain replacement commits the bearer, verified pin and reference to a separate native
// signing key together. Old separate credential/signing entries remain readable until re-pair.
const PROFILE_KEY = 'st3.device.profile';
type PhoneSigning = DeviceKey & { handle?: string };
type PhoneProfile = { url: string; credential: string; signing?: PhoneSigning; personRootFingerprint?: string };
// Glasses are an experiment (mission fleet/stui/glass): off unless the person turns them on here.
const GLASSES_KEY = 'st3.experiments.glasses';
// Simplified conversations: this phone's own choice, on unless turned off, never synced.
const SIMPLE_KEY = 'st3.conversation.simple';

/** An error as a person reads it: st's errors in plain words (the SDK's plainError). */
export function errorText(error: unknown): string {
  if (error instanceof ClientError) {
    const refused = signatureRefusal(error.response.code, 'phone');
    if (refused) return refused;
  }
  return plainError(error);
}

/** `parameters` for message.send `id`, signed with this phone's key when it has an enrolled one. */
async function signMessage<P extends { to: string; content: string; session_id?: string; tags?: string[] }>(id: string, parameters: P): Promise<P> {
  const profile = await SecureStore.getItemAsync(PROFILE_KEY);
  const stored = profile ? null : await SecureStore.getItemAsync(SIGNING_KEY);
  const signing: PhoneSigning | undefined = profile ? (JSON.parse(profile) as PhoneProfile).signing : stored ? JSON.parse(stored) : undefined;
  if (!signing) return parameters;
  const digest = await Crypto.digestStringAsync(Crypto.CryptoDigestAlgorithm.SHA256, id, { encoding: Crypto.CryptoEncoding.HEX });
  const message: Unsigned = {
    subject: messageSubject(digest),
    signer: signing.person,
    fields: { content: parameters.content, from: signing.person, in_reply_to: null, session_id: parameters.session_id ?? null, tags: parameters.tags ?? [], title: null, to: parameters.to },
    key: signing.key,
    chain: signing.chain,
    // A fresh one-time number for each new send; a retry of the same send reuses the request.
    nonce: base64url(Crypto.getRandomBytes(16)),
    signedAt: Date.now(),
  };
  return { ...parameters, signature: signatureParameter(message, await signWithDeviceKey(signedBytes(message), signing.handle)) };
}
function currentAgent(agent: { operational?: { layer?: string } }) { return agent.operational?.layer !== 'history'; }
function items<K extends Resource['kind']>(page: { items: Resource[] }, kind: K): Extract<Resource, { kind: K }>[] {
  return page.items.filter((item): item is Extract<Resource, { kind: K }> => item.kind === kind);
}
function actionId() { return `action/ios-${Crypto.randomUUID()}`; }

function useAppStore(proof?: FabricProfile) {
  const proofRef = useRef(proof); proofRef.current = proof;
  /** The snapshot the windows last showed: a terminal key may be fenced by it, since any earlier one of this host is accepted. */
  const lastSnapshot = useRef('');
  const [order, setOrder] = useState<Tab[]>(tabOrder(null));
  const [url, setUrl] = useState(proof?.url ?? ''), [urlDraft, setUrlDraft] = useState('');
  const [credential, setCredential] = useState<string | null>(proof?.credential ?? null);
  const [pairDraft, setPairDraft] = useState<{ gateway: string; id: string; code: string } | null>(null);
  const [data, setData] = useState<Data>(emptyData);
  const [truncated, setTruncated] = useState<Partial<Record<keyof Data, boolean>>>({});
  const [loadErrors, setLoadErrors] = useState<Partial<Record<keyof Data, string>>>({});
  const foreground = useRef(new ForegroundGate(proof && !proof.ready ? 'background' : AppState.currentState));
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
  // Attention the person acted on from here: st closing it is their doing, not a vanishing.
  const acted = useRef(new Set<string>());
  // Whether this connection has delivered its first attention window: what a stored copy held
  // and st no longer lists was not on screen while it closed, so only later windows keep items.
  const attentionLive = useRef(false);
  const missionDetailCache = useRef(new Map<string, Mission>());
  const client = useMemo(() => url ? new St3Client({ baseUrl: url, credential: () => credential ?? undefined, fetchImpl: gatewayFetch(), client: clientName('smalltalk-ios', app.expo.version, process.env.EXPO_PUBLIC_ST3_BUILD) }) : null, [url, credential]);
  // Image bytes go up through Expo's fetch: React Native's cannot send a byte array as a body.
  const uploader = useMemo(() => url ? new St3Client({ baseUrl: url, credential: () => credential ?? undefined, fetchImpl: gatewayFetch(expoFetch as unknown as typeof fetch), client: clientName('smalltalk-ios', app.expo.version, process.env.EXPO_PUBLIC_ST3_BUILD) }) : null, [url, credential]);

  useEffect(() => { if (proof) return; Promise.allSettled([AsyncStorage.getItem(URL_KEY), AsyncStorage.getItem(ORDER_KEY), SecureStore.getItemAsync(CREDENTIAL_KEY), AsyncStorage.getItem(PROJECTION_CACHE_KEY), SecureStore.getItemAsync(PROFILE_KEY)]).then(([u, o, c, p, saved]) => {
    let paired: PhoneProfile | undefined;
    if (saved.status === 'fulfilled' && saved.value) {
      try { paired = JSON.parse(saved.value); } catch { setError('The saved device profile could not be read.'); return; }
    }
    if (saved.status === 'rejected') { setError('Secure credential storage is unavailable on this build.'); return; }
    const gateway = paired?.url ?? (u.status === 'fulfilled' ? u.value : null);
    const bearer = paired?.credential ?? (c.status === 'fulfilled' ? c.value : null);
    if (gateway) { setUrl(gateway); setUrlDraft(gateway); }
    // A stored order may use earlier tab names; they still count.
    if (o.status === 'fulfilled' && o.value) { try { setOrder(tabOrder(JSON.parse(o.value))); } catch { /* use default */ } }
    if (bearer) {
      if (gateway && p.status === 'fulfilled') {
        const cache = hydrateProjectionForPairedDevice(p.value, gateway, true);
        if (cache) { setData(cache.data); setTruncated(Object.fromEntries(cache.truncated.map(key => [key, true]))); setHasSynced(true); setStatus('connecting'); cachedActor.current = cache.actor; cacheSavedAt.current = cache.savedAt; setCachedHostId(cache.hostId); }
      }
      setCredential(bearer);
    }
    if (c.status === 'rejected') setError('Secure credential storage is unavailable on this build.');
  }); }, []);

  useEffect(() => {
    if (!proof) return;
    setUrl(proof.url); setCredential(proof.credential);
    foreground.current.update('background');
  }, [proof?.url, proof?.credential]);

  function clearCaches() {
    cachedActor.current = ''; cacheSavedAt.current = 0;
    conversationCache.current.clear(); draftCache.current.clear(); missionDetailCache.current.clear();
    setData(emptyData); setTruncated({}); setHasSynced(false); setCachedHostId(''); setSnapshot(null);
  }
  async function clearCachedProjection() { cacheGeneration.current++; clearCaches(); if (!proof) await AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {}); }

  // Capabilities once per connection: limits, the session's actor, and what it may control.
  const loadCapabilities = useCallback(async () => {
    if (!client) return;
    try {
      const capability = await client.capabilities();
      if (cachedActor.current && cachedActor.current !== capability.value.session_actor) { clearCaches(); if (!proofRef.current) void AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {}); }
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
    attentionLive.current = false;
    const opened = new Feed(client, {
      onWindow: (name, rows, hasMore, at) => {
        if (!current()) return;
        if (at?.id) lastSnapshot.current = at.id;
        // Home decides what of attention to show, as stui does; agents drop only history.
        const shown = name === 'agents' ? (rows as Array<{ operational?: { layer?: string } }>).filter(currentAgent) : rows;
        // Nothing leaves Home by itself: what st closes while it is shown stays until cleared.
        const seen = attentionLive.current;
        if (name === 'attention') attentionLive.current = true;
        setData(previous => ({ ...previous, [name]: name === 'attention' && seen ? keepClosed(previous.attention, shown as Attention[], acted.current, capsRef.current?.session_actor) : shown }));
        setTruncated(previous => ({ ...previous, [name]: hasMore }));
        setLoadErrors(previous => { if (!(name in previous)) return previous; const rest = { ...previous }; delete rest[name]; return rest; });
        proofRef.current?.record('window', { name, rows: shown.length });
        setSnapshot(at); setCachedHostId(at.host_id); setHasSynced(true);
      },
      onConnection: (state, issue) => {
        if (!current()) return;
        proofRef.current?.record('feed', { state });
        setStatus(state === 'live' ? 'online' : state === 'connecting' ? 'connecting' : 'offline');
        setConnectionIssue(issue ?? '');
        if (state === 'live') { setError(''); void loadCapabilities(); }
      },
      onWindowError: (name, message) => { if (current()) setLoadErrors(previous => ({ ...previous, [name]: message })); },
      onConversationFrame: (rows, replace) => { if (current()) proofRef.current?.record('conversation', { rows, replace }); },
    }, foreground.current, actionId);
    // Paired: connecting from here on, even while the app waits to be active before it dials.
    setStatus(previous => previous === 'setup' ? 'connecting' : previous);
    setFeed(opened);
    return () => { opened.close(); setFeed(held => held === opened ? null : held); };
  }, [client, credential, loadCapabilities]);
  useEffect(() => {
    if (proof) foreground.current.update(proof.ready && url === proof.url ? AppState.currentState : 'background');
  }, [url, proof?.url, proof?.ready]);
  useEffect(() => {
    const subscription = AppState.addEventListener('change', state => foreground.current.update(proofRef.current && !proofRef.current.ready ? 'background' : state));
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
    if (proof || !hasSynced || !snapshot || !caps) return;
    const timer = setTimeout(() => {
      cacheSavedAt.current = Date.now();
      const truncatedKeys = (Object.keys(truncated) as Array<keyof Data>).filter(key => truncated[key]);
      const encoded = encodeProjectionCache(url, caps.session_actor, snapshot.host_id, snapshot.store_index, { ...data, attention: data.attention.filter(item => !item.closedElsewhere) }, cacheSavedAt.current, truncatedKeys);
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
      return listCollectionPages(options => list(options), limit).then(result => ({ rows: mergedRows(result.pages.flatMap(page => page.value.items.filter(item => (item as { kind: string }).kind === kind)) as Array<{ id?: string }>) as unknown as Data[OnDemand], truncated: result.truncated }));
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
    if (status !== 'online' || (proof && !proof.ready)) { setError('Still connecting; try again in a moment.'); return false; }
    setBusy(true);
    try { await retryTransient(8, action, notApplied); setError(''); await loadLists(reload); return true; } catch (e) { setError(errorText(e)); return false; } finally { setBusy(false); }
  }

  async function completePairing(id: string, code: string, fingerprint: string, unpinned: boolean, gateway: string) {
    if (proof) throw new Error('Close the fabric proof to change ordinary device pairing.');
    const pin = validatePairingTrust(fingerprint, unpinned);
    if (!canVerifyPairing()) throw new Error('This build cannot verify pairing proofs; update the phone app.');
    const oldRaw = await SecureStore.getItemAsync(PROFILE_KEY);
    const old: PhoneProfile | undefined = oldRaw ? JSON.parse(oldRaw) : undefined;
    // Anonymous discovery reveals no key material and never receives an old bearer.
    const requestFetch = gatewayFetch();
    const response = await requestFetch(`${gateway}/v1/client/capabilities`);
    const text = await response.text();
    if (!response.ok || text.length > 8192) throw new Error('Member cannot advertise pairing proofs; upgrade it. The code was not submitted.');
    let advertisement: { api_version?: string; capabilities?: Array<{ id: string; version: number; state: string }> };
    try { advertisement = JSON.parse(text); } catch { throw new Error('Invalid pairing advertisement. The code was not submitted.'); }
    if (advertisement.api_version !== API_VERSION || !Array.isArray(advertisement.capabilities) || !advertisement.capabilities.some(c => c.id === 'device-key-proofs' && c.version === 1 && c.state === 'granted')) {
      throw new Error('Member does not support verifiable device grants; upgrade it. The code was not submitted.');
    }
    const made = await createDeviceKey();
    if (!made) throw new Error('This build cannot make a device key; update the phone app.');
    let consumed = false;
    let deviceId = '';
    try {
      const gatewayClient = new St3Client({ baseUrl: gateway, fetchImpl: requestFetch, client: clientName('smalltalk-ios', app.expo.version, process.env.EXPO_PUBLIC_ST3_BUILD) });
      const result = await gatewayClient.completePairing(id, { api_version: API_VERSION, code, device_public_key: made.key, key_storage: made.storage });
      consumed = true; deviceId = result.value.device_id;
      await verifyPairing(result.value, made.key, pin, {
        hash: async bytes => new Uint8Array(await Crypto.digest(Crypto.CryptoDigestAlgorithm.SHA256, new Uint8Array(bytes))),
        verify: verifyGrantSignature,
      });
      const signs = result.value.scopes.includes('control.messages');
      const next: PhoneProfile = { url: gateway, credential: result.value.credential,
        ...(pin ? { personRootFingerprint: pin } : {}),
        ...(signs ? { signing: { key: made.key, storage: made.storage, handle: made.handle, person: result.value.person_id, chain: result.value.device_key_chain! } } : {}),
      };
      // This single secure write is the commit. Before it, the old bearer, key and pin survive.
      await SecureStore.setItemAsync(PROFILE_KEY, JSON.stringify(next), { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY });
      if (!signs) await removeDeviceKey(made.handle).catch(() => {});
      if (old?.signing?.handle) await removeDeviceKey(old.signing.handle).catch(() => {});
      if (!oldRaw) await removeDeviceKey().catch(() => {});
      await SecureStore.deleteItemAsync(CREDENTIAL_KEY).catch(() => {});
      await SecureStore.deleteItemAsync(SIGNING_KEY).catch(() => {});
      await clearCachedProjection().catch(() => {});
      setUrl(gateway); setUrlDraft(gateway); setCredential(next.credential); setPairDraft(null); setPairingIssue(''); setError('');
    } catch (error) {
      await removeDeviceKey(made.handle).catch(() => {});
      if (consumed) {
        const safeId = /^device\/[0-9a-f]{24}$/.test(deviceId) ? deviceId : 'the new device';
        throw new Error(`${errorText(error)} The code was consumed; inspect and revoke ${safeId} on the trusted member before pairing again. The previous profile was retained.`);
      }
      throw error;
    }
  }

  const actions = {
    async saveUrl() {
      if (proof) { setError('Close the fabric proof to change the saved gateway.'); return; }
      const normalized = normalizeGatewayUrl(urlDraft);
      if (!normalized) { setError('Enter the paired gateway HTTPS URL, or http:// with a Tailscale address (100.x), a .local name, or a private LAN address (10.x, 172.16-31.x, 192.168.x).'); return; }
      if (normalized !== url) await clearCachedProjection();
      const saved = await SecureStore.getItemAsync(PROFILE_KEY);
      if (saved) await SecureStore.setItemAsync(PROFILE_KEY, JSON.stringify({ ...JSON.parse(saved), url: normalized }), { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY });
      await AsyncStorage.setItem(URL_KEY, normalized); setUrl(normalized); setError('');
    },
    async pair(id: string, code: string, fingerprint: string, unpinned = false): Promise<boolean> {
      const gateway = pairDraft?.gateway ?? url;
      if (!gateway || !id.trim() || !code.trim()) return false;
      setBusy(true);
      try { await completePairing(id.trim(), code.trim(), fingerprint, unpinned, gateway); return true; } catch (e) { setError(errorText(e)); return false; } finally { setBusy(false); }
    },
    /** A Debug pairing link: the gateway, pairing id, and code all in one. */
    async pairFromLink(gatewayRaw: string, id: string, code: string) {
      const gateway = normalizeGatewayUrl(gatewayRaw);
      if (!gateway) return;
      setPairDraft({ gateway, id, code });
      const saved = await SecureStore.getItemAsync(PROFILE_KEY);
      const pin = saved ? (JSON.parse(saved) as PhoneProfile).personRootFingerprint : undefined;
      setPairingIssue(credential && !pin ? REPAIR_WARNING : 'Enter the fingerprint copied separately from the trusted machine before completing this pairing.');
    },
    cancelPairDraft() { setPairDraft(null); setPairingIssue(''); },
    async forget() {
      if (proof) { proof.close(); return; }
      const saved = await SecureStore.getItemAsync(PROFILE_KEY);
      const profile: PhoneProfile | undefined = saved ? JSON.parse(saved) : undefined;
      await SecureStore.deleteItemAsync(PROFILE_KEY);
      if (profile?.signing?.handle) await removeDeviceKey(profile.signing.handle);
      await SecureStore.deleteItemAsync(CREDENTIAL_KEY); await SecureStore.deleteItemAsync(SIGNING_KEY); await removeDeviceKey(); await clearCachedProjection();
      setCredential(null); setCaps(null); setPairDraft(null); setPairingIssue(profile?.personRootFingerprint ? '' : REPAIR_WARNING); setHistoricalSessions([]); setStatus('setup');
    },
    /** Complete a person step; `answer` is a structured request's named answer, by id. */
    async done(item: Attention, summary: string, answer?: string) {
      if (!client) return false;
      acted.current.add(item.id);
      const typed = personAnswer(item.request, answer, summary);
      if (typeof typed === 'string') { setError(typed); return false; }
      return runAction(async () => { const id = actionId(); return client.workDone({ id, idempotency_key: id, fence: await fence({ [item.id]: item.revision }), parameters: { target_id: item.source_id, episode: item.episode || item.revision, summary, ...(typed ? { answer: typed } : {}) } }); });
    },
    /** Clear an item st closed: only the person's own word removes it from Home. */
    clearClosed(id: string) {
      acted.current.add(id);
      setData(previous => ({ ...previous, attention: previous.attention.filter(item => item.id !== id) }));
    },
    /** Mark a message read from Home: it leaves the person's attention. */
    async markRead(item: Attention) {
      if (!client) return false;
      acted.current.add(item.id);
      return runAction(async () => { const id = actionId(); return client.messageRead({ id, idempotency_key: id, fence: await fence({ [item.id]: item.revision }), parameters: { target_id: item.source_id } }); });
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
      if (status !== 'online' || (proof && !proof.ready)) return 'offline';
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
      const began = performance.now();
      try {
        // Each try is a new request on a fresh fence, made only after st said the last applied nothing.
        await retryTransient(8, async () => {
          const id = actionId();
          const unsigned = { to, content, ...(sessionId ? { session_id: sessionId } : {}), ...(tags?.length ? { tags } : {}), ...(attachments.length ? { attachments } : {}) };
          const parameters = proof ? unsigned : await signMessage(id, unsigned);
          const request = { id, idempotency_key: id, fence: await fence(), parameters };
          try {
            await client.messageSend(request);
          } catch (e) {
            if (e instanceof ClientError || !isTransient(e)) throw e;
            // st's answer was lost, so the message may have arrived. The identical request (same
            // key, signature and nonce) gets st's first answer, never a second message.
            await new Promise(resolve => setTimeout(resolve, 1000));
            await client.messageSend(request);
          }
        }, notApplied);
        proof?.record('message-accepted', { elapsedMs: performance.now() - began });
        return null;
      } catch (e) { proof?.record('message-failed', { elapsedMs: performance.now() - began }); return errorText(e); }
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
      // Typed keys need no screen fence, and an agent's spinner moves the screen faster than a read
      // and a send can race it (Nathan, 2026-10-06: nothing typed ever arrived). So one request,
      // on the snapshot the windows last showed (any earlier one of this host is accepted), fenced
      // by the incarnation only. An st that still asks for the screen's sequence says so, and the
      // old way is used.
      if ((mode === 'raw' || mode === 'key') && lastSnapshot.current) {
        const id = actionId();
        try {
          await client.terminalInput({ id, idempotency_key: id, fence: { snapshot_id: lastSnapshot.current, subject_revisions: {}, runtime_incarnation: incarnation } as TerminalFence, parameters: { terminal_id: terminalId, mode, value } });
          return;
        } catch (error) { if (!wantsScreenSequence(error)) throw error; }
      }
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
      if (!proof) void AsyncStorage.setItem(ORDER_KEY, JSON.stringify(updated));
    },
    reconnect() { feed?.reconnect(); },
    setSimpleOn(on: boolean) {
      setSimpleOn(on);
      if (!proof) void AsyncStorage.setItem(SIMPLE_KEY, on ? '1' : '0').catch(() => {});
    },
    setGlassesOn(on: boolean) {
      setGlassesOn(on);
      if (!proof) void AsyncStorage.setItem(GLASSES_KEY, on ? '1' : '0').catch(() => {});
    },
  };

  const knownHostId = snapshot?.host_id ?? cachedHostId;
  const gatewayMachineId = knownHostId ? `machine/${knownHostId.replace(/^host\//, '')}` : '';
  const gatewayHost = data.machines.find(m => m.id === gatewayMachineId)?.name ?? (knownHostId ? knownHostId.replace(/^host\//, '') : '');
  const canControlTerminal = caps?.capabilities.some(capability => capability.id === 'terminal.input' && capability.state === 'granted') ?? false;

  return {
    order, url, urlDraft, setUrlDraft, credential, data, truncated, loadErrors, feed, connectionIssue, caps, snapshot, status, hasSynced,
    error, setError, pairingIssue, setPairingIssue, pairDraft, busy, historicalSessions, conversationCache, draftCache, client,
    gatewayMachineId, gatewayHost, canControlTerminal, loadLists, actions,
    treeView, setTreeView, scrollRequest, requestScroll: (y: number) => setScrollRequest({ y, at: Date.now() }),
    glassesOn, glassesGranted, glasses, glassesIssue, simpleOn,
  };
}

export type Store = ReturnType<typeof useAppStore>;
const StoreContext = createContext<Store | null>(null);
export function StoreProvider({ children, proof }: { children: ReactNode; proof?: FabricProfile }) {
  const store = useAppStore(proof);
  return <StoreContext.Provider value={store}>{children}</StoreContext.Provider>;
}
export function useStore(): Store {
  const store = useContext(StoreContext);
  if (!store) throw new Error('useStore outside StoreProvider');
  return store;
}

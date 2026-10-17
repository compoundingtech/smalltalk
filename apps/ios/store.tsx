import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { AppState } from 'react-native';
import AsyncStorage from '@react-native-async-storage/async-storage';
import * as SecureStore from 'expo-secure-store';
import * as Crypto from 'expo-crypto';
import { submitAnswer } from './answer';
import { resendMessage, sendMessage, type SendOutcome } from './messageSend';
import { API_VERSION, ClientError, St3Client, isTransient, notApplied, plainError, promptGone, retryTransient, type Arrangement, type ArrangementOperation, type Runtime, type Attention, type AttachmentInput, type Capabilities, type ConversationSearch, type Glass, type Launch, type LaunchVariant, type Mission, type Resource, type ResourceObservation, type Snapshot, type TimelineEntry } from '../../clients/typescript/st3-client';
import { keepClosed, personAnswer, promptAnswers, clientName, isSnapshotChurn, listSessionPages, OLDER_PAGE, readOlder, type Conversation, type Older, type SessionView, base64url, messageSubject, signatureParameter, signatureRefusal, signedBytes, signedMessageFields, isLongMessage, type DeviceKey, type Unsigned } from '@smalltalk/st3-views';
import app from './app.json';
import { canVerifyPairing, createDeviceKey, removeDeviceKey, signWithDeviceKey, verifyGrantSignature } from './modules/st-device-key';
import { REPAIR_WARNING, validatePairingTrust, verifyPairing } from './pairingProof';
import { emptyData, encodeProjectionCache, hydrateProjectionForPairedDevice, PROJECTION_CACHE_KEY, type Data } from './projectionCache';
import { listCollectionPages, mergedRows } from './collectionPages';
import { rememberBounded } from './boundedCache';
import { wantsScreenSequence, withFreshTerminalFence, type TerminalFence } from './terminalControls';
import { sendFences } from './sendFence';
import { Feed } from './feed';
import { watchSync, type SyncRead } from './syncWatch';
import { quality, localDrafts } from './qualityNative';
import { nativeCollectionSocket } from './modules/st-collections';
import { FabricCarrier, routeLabel, selectRoute, type CarrierChoice, type FabricSnapshot, type Route } from './carrier';
import { Observations, toObservationReport, type Context } from './observations';
import { buildFabricDefault, builtFabricMembers, credentialFor, defaultMemberName, fabricGatewayId, fabricMembers, fabricTargetFromText, isFabricGatewayId, MAX_FABRIC_MEMBERS, memberName, memberPairing, memberProfileKey, readFabricMembers, sameFabricTarget, sameName, storedFabricMembers, walkableMembers, type FabricMember } from './fabricTarget';
import { fabricAvailable, fabricIdentity, fabricPathInUse, nativeCarrierPorts } from './fabricBridge';
import { ForegroundGate } from './foreground';
import type { FabricProfile } from './fabricProof';
import { gatewayFetch } from './gatewayFetch';
import { normalizeGatewayUrl } from './gatewayUrl';
import { tabOrder, type Tab } from './tabs';
import type { SidebarResource } from './sidebarView';
import { readSidebarResources } from './sidebarResources';
import { SIDEBAR_RETIRED, sidebarArrangement, sidebarRetired, sidebarSubject } from './sidebarEdits';
import { fetch as expoFetch } from 'expo/fetch';
import { decodeBase64, encodeBase64, type Picked } from './images';

// Everything the screens share: the paired gateway, the one collections socket, the lists it keeps
// current, the lists a screen loads when it opens, and the actions. Screens follow a conversation
// or a terminal themselves, on this socket, while they are open.

export type OnDemand = 'launches' | 'machines' | 'devices' | 'sessions';
export type Status = 'setup' | 'connecting' | 'online' | 'offline';
export type Planner = 'codex' | 'claude' | 'pi' | 'omp' | 'opencode';
/** A message.send request as sent, kept whole so an unconfirmed one can be sent again unchanged. */
export type MessageRequest = Parameters<St3Client['messageSend']>[0];
/** What became of a message: sent, unconfirmed (with the exact request to send again), or failed as a person reads it. */
export type Sent = { how: 'sent' } | { how: 'unconfirmed'; request: MessageRequest } | { how: 'failed'; error: string };

const URL_KEY = 'st3.gateway.url', ORDER_KEY = 'st3.tabs.order.resources-v1', CREDENTIAL_KEY = 'st3.device.credential';
// The enrolled signing key's public half, the person it signs as and its grant chain (the private
// half stays in the native module).
const SIGNING_KEY = 'st3.device.signing';
// One Keychain replacement commits the bearer, verified pin and reference to a separate native
// signing key together. Old separate credential/signing entries remain readable until re-pair.
const PROFILE_KEY = 'st3.device.profile';
type PhoneSigning = DeviceKey & { handle?: string };
type PhoneProfile = { url: string; credential: string; signing?: PhoneSigning; personRootFingerprint?: string };
// Simplified conversations: this phone's own choice, on unless turned off, never synced.
const SIMPLE_KEY = 'st3.conversation.simple';
// The carrier: the saved gateway (default) or fabric, an opt-in; the saved fabric members, in the
// person's order; and whether a fabric that cannot be used falls back to the saved gateway. The list
// has its own key; the old single-target key keeps the member that uses this device's own pairing,
// which an older app build reads.
const OBSERVATIONS_KEY = 'st3.observations';
const CARRIER_KEY = 'st3.carrier', FABRIC_TARGET_KEY = 'st3.fabric.target', FABRIC_MEMBERS_KEY = 'st3.fabric.members', FABRIC_FALLBACK_KEY = 'st3.fabric.fallback';

/** An error as a person reads it: st's errors in plain words (the SDK's plainError). */
export function errorText(error: unknown): string {
  if (error instanceof ClientError) {
    const refused = signatureRefusal(error.response.code, 'phone');
    if (refused) return refused;
  }
  return plainError(error);
}

/** `parameters` for message.send `id`, signed with this phone's key when it has an enrolled one. */
async function signMessage<P extends { to: string; content: string; session_id?: string; tags?: string[] }>(id: string, parameters: P, profileKey = PROFILE_KEY): Promise<P> {
  // A member paired on its own signs with its own key; only this device's own profile has an older separate entry.
  const profile = await SecureStore.getItemAsync(profileKey);
  const stored = profile || profileKey !== PROFILE_KEY ? null : await SecureStore.getItemAsync(SIGNING_KEY);
  const signing: PhoneSigning | undefined = profile ? (JSON.parse(profile) as PhoneProfile).signing : stored ? JSON.parse(stored) : undefined;
  if (!signing) return parameters;
  const digest = await Crypto.digestStringAsync(Crypto.CryptoDigestAlgorithm.SHA256, id, { encoding: Crypto.CryptoEncoding.HEX });
  const message: Unsigned = {
    subject: messageSubject(digest),
    signer: signing.person,
    // Past 8 KiB the claim keeps a preview, so the signature covers that with the text's hash and length.
    fields: signedMessageFields(
      { from: signing.person, in_reply_to: null, session_id: parameters.session_id ?? null, tags: parameters.tags ?? [], title: null, to: parameters.to },
      parameters.content,
      isLongMessage(parameters.content) ? await Crypto.digestStringAsync(Crypto.CryptoDigestAlgorithm.SHA256, parameters.content, { encoding: Crypto.CryptoEncoding.HEX }) : undefined,
    ),
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
function sentAs(outcome: SendOutcome<MessageRequest>): Sent { return outcome.how === 'failed' ? { how: 'failed', error: errorText(outcome.error) } : outcome; }

/** Where a measurement was served from: the route it ran over and, for fabric, the path once measured. */
function observedContext(route: Route, fabricPath: 'direct' | 'relay' | 'unknown'): Context {
  switch (route.kind) {
    case 'fabric': return { carrier: 'fabric', ...(fabricPath === 'direct' || fabricPath === 'relay' ? { path: fabricPath } : {}) };
    case 'lan': return { carrier: 'lan' };
    case 'https': return { carrier: 'https' };
    case 'tailnet': return { carrier: 'tailscale' };
    // No route chosen yet: whatever is measured now is not a measurement of any carrier.
    default: return { carrier: 'none' };
  }
}

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
  // The page `sync` notice (this host lacks records a peer holds): the socket's frames carry none, so a
  // small page read supplies it while online. Unread is `undefined`, and level with its peers is `null`.
  const [sync, setSync] = useState<SyncRead>(undefined);
  // A read that failed while a behind notice was up: that notice stays, marked as not refreshed.
  const [syncStale, setSyncStale] = useState(false);
  // Whether the missions window (followed only while a missions screen shows) has delivered its rows.
  const [missionsLoaded, setMissionsLoaded] = useState(false);
  const [error, setError] = useState(''), [pairingIssue, setPairingIssue] = useState(''), [busy, setBusy] = useState(false);
  const [historicalSessions, setHistoricalSessions] = useState<SessionView[]>([]);
  // The Agents list shows a list or a tree, as stui's `t` toggles.
  const [treeView, setTreeView] = useState(false);
  const [simpleOn, setSimpleOn] = useState(true);
  const [arrangements, setArrangements] = useState<Arrangement[]>([]), [arrangementsIssue, setArrangementsIssue] = useState('');
  const [sidebarTerminals, setSidebarTerminals] = useState<Runtime[]>([]), [sidebarTerminalsIssue, setSidebarTerminalsIssue] = useState('');
  const [glasses, setGlasses] = useState<Glass[]>([]), [glassesIssue, setGlassesIssue] = useState('');
  const [scrollRequest, setScrollRequest] = useState<{ y: number; at: number } | null>(null);
  const [sidebarResources, setSidebarResources] = useState<ResourceObservation[]>([]), [sidebarResourcesIssue, setSidebarResourcesIssue] = useState('');
  const sidebarResourcesRead = useRef<{ focused?: string; promise: Promise<void> } | null>(null);
  const sidebarCreation = useRef<{ subject: string; created: boolean } | null>(null);
  const sidebarRemoved = useRef(false);
  const sidebarWriting = useRef(false);
  const sidebarSeen = useRef(new Map<string, SidebarResource>());
  const cachedActor = useRef(''), cacheSavedAt = useRef(0), cacheGeneration = useRef(0);
  const conversationCache = useRef(new Map<string, Conversation<TimelineEntry>>()), draftCache = useRef(new Map<string, string>());
  const [draftsReady, setDraftsReady] = useState(0);
  const draftsOwner = useRef('');
  useEffect(() => {
    const owner = caps?.session_actor;
    if (proof || !owner || draftsOwner.current === owner) return;
    draftsOwner.current = owner;
    let current = true;
    void quality.ready.then(() => localDrafts.restore(owner, draftCache.current, () => current && draftsOwner.current === owner)).then(restored => {
      if (!current) return;
      if (quality.wasUnclean) quality.counters.add('drafts_restored_unclean', restored);
      setDraftsReady(n => n + 1);
    });
    return () => { current = false; };
  }, [proof, caps?.session_actor]);
  useEffect(() => foreground.current.subscribe(active => { if (!active && !proofRef.current) void localDrafts.save(draftsOwner.current, draftCache.current); }), []);
  // Images messages carry, as data URIs, so a conversation scrolled back to does not read them again.
  const imageCache = useRef(new Map<string, Promise<string>>());
  // The request of a send st never confirmed, so sending the same message again repeats it instead of making a second.
  // Attention the person acted on from here: st closing it is their doing, not a vanishing.
  const acted = useRef(new Set<string>());
  // Whether this connection has delivered its first attention window: what a stored copy held
  // and st no longer lists was not on screen while it closed, so only later windows keep items.
  const attentionLive = useRef(false);
  const missionDetailCache = useRef(new Map<string, Mission>());
  // The carrier. The saved gateway `url` is this device's identity and its fallback route; the URL the
  // client uses is the saved gateway, or, with fabric chosen and ready, the native bridge's own listener.
  // Fabric is offered only where this build links the bridge.
  const [carrierChoice, setCarrierChoice] = useState<CarrierChoice>('tailscale');
  const [fabricMemberList, setFabricMemberList] = useState<FabricMember[]>([]);
  // The members paired on their own, by Keychain key: each one's bearer goes only to that member.
  const [ownProfiles, setOwnProfiles] = useState<ReadonlyMap<string, PhoneProfile>>(new Map());
  // The member the person is pairing this phone with, while the pairing form is open.
  const [pairingMember, setPairingMember] = useState<FabricMember | null>(null);
  const fabricTarget = fabricMemberList.find(member => member.device) ?? fabricMemberList[0] ?? null;
  const ownKeys = useMemo(() => new Set(ownProfiles.keys()), [ownProfiles]);
  // The carrier walks only members this phone is paired with. A device not paired yet may reach its
  // own member to pair over fabric alone: pairing sends no credential.
  const carrierMembers = useMemo(
    () => (credential ? walkableMembers(fabricMemberList, true, ownKeys) : fabricTarget ? [fabricTarget] : []),
    [credential, fabricMemberList, ownKeys, fabricTarget],
  );
  const [fallbackOn, setFallbackOn] = useState(true);
  const [fabricBuilt, setFabricBuilt] = useState(false);
  const [fabricNode, setFabricNode] = useState('');
  const [fabricPath, setFabricPath] = useState<'direct' | 'relay' | 'unknown'>('unknown');
  const [fabricSnapshot, setFabricSnapshot] = useState<FabricSnapshot>({ phase: 'off', bridgeUrl: '', path: 'unknown', reason: '', member: '', tried: 0 });
  const carrierRef = useRef<FabricCarrier | null>(null);
  if (!carrierRef.current) carrierRef.current = new FabricCarrier(nativeCarrierPorts(), setFabricSnapshot);
  const carrier = carrierRef.current;
  useEffect(() => { if (!proof) carrier.configure(fabricBuilt && carrierChoice === 'fabric', carrierMembers); }, [proof, carrier, fabricBuilt, carrierChoice, carrierMembers]);
  useEffect(() => {
    if (proof) return;
    carrier.foreground(foreground.current.active);
    return foreground.current.subscribe(active => carrier.foreground(active));
  }, [proof, carrier]);
  // The member fabric is connected to, and the profile whose credential and key go to it: this
  // device's own for its member, that member's own otherwise, and none for a member not paired yet.
  const fabricMember = fabricSnapshot.phase === 'ready' ? fabricMemberList.find(member => member.name === fabricSnapshot.member) ?? null : null;
  const memberProfile = fabricMember && !fabricMember.device ? ownProfiles.get(memberProfileKey(fabricMember)) ?? null : null;
  const selected = useMemo(() => {
    if (proof) return { baseUrl: url || null, route: { kind: 'none', fellBack: false, why: '' } as Route, pending: false, issue: '' };
    const chosen = selectRoute(fabricBuilt ? carrierChoice : 'tailscale', fallbackOn, url && !isFabricGatewayId(url) ? url : null, fabricSnapshot, carrierMembers.length > 0);
    // Connected to a member only to pair with it, or to one this render cannot name: no credential is
    // sent there. The saved gateway, as after any fabric failure, carries the device meanwhile.
    if (chosen.route.kind === 'fabric' && (!fabricMember || (!fabricMember.device && !memberProfile))) {
      const waiting: FabricSnapshot = { ...fabricSnapshot, phase: 'failed', reason: fabricMember ? `${fabricMember.name} is not paired with this phone yet.` : 'Fabric is changing members.' };
      return selectRoute(carrierChoice, fallbackOn, url && !isFabricGatewayId(url) ? url : null, waiting, true);
    }
    return chosen;
  }, [proof, url, fabricBuilt, carrierChoice, fallbackOn, fabricSnapshot, carrierMembers.length, fabricMember, memberProfile]);
  // Over fabric, this device's credential goes only to its own member and a member's own only to that member.
  const clientCredential = credentialFor(selected.route.kind === 'fabric', fabricMember, memberProfile?.credential, credential);
  const signingKeyRef = useRef(PROFILE_KEY);
  signingKeyRef.current = selected.route.kind === 'fabric' && memberProfile && fabricMember ? memberProfileKey(fabricMember) : PROFILE_KEY;
  const routeRef = useRef<Route>(selected.route); routeRef.current = selected.route;
  const feedMisses = useRef(0);
  // What the phone measures of its own service (see observations.ts): timed where the app already
  // waits, kept on the device, and reported to the member by the reporter.
  const fabricPathRef = useRef<'direct' | 'relay' | 'unknown'>('unknown');
  fabricPathRef.current = selected.route.kind === 'fabric' ? fabricPath : 'unknown';
  const observationsRef = useRef<Observations | null>(null);
  if (!observationsRef.current) {
    observationsRef.current = new Observations(
      { read: () => AsyncStorage.getItem(OBSERVATIONS_KEY), write: value => AsyncStorage.setItem(OBSERVATIONS_KEY, value) },
      { now: () => Date.now(), id: () => Crypto.randomUUID() },
      () => observedContext(routeRef.current, fabricPathRef.current),
    );
  }
  const observations = observationsRef.current;
  const timers = useRef<{ openToLive: number | null; connect: number | null; recover: number | null; everLive: boolean }>({ openToLive: null, connect: null, recover: null, everLive: false });
  useEffect(() => { if (!proof) void observations.load(); }, [proof, observations]);
  useEffect(() => {
    if (proof) return;
    const gate = foreground.current;
    const changed = (active: boolean) => {
      observations.setForeground(active);
      const held = timers.current;
      if (active) {
        if (held.openToLive === null) held.openToLive = observations.begin('ios-open-to-live');
      } else {
        // A foreground that ended before the feed was live, a connection that was never made, and a
        // recovery that was cut off are not measurements.
        for (const key of ['openToLive', 'connect', 'recover'] as const) { const token = held[key]; if (token !== null) { observations.cancel(token); held[key] = null; } }
        observations.setLive(false);
        void observations.flush();
      }
    };
    changed(gate.active);
    return gate.subscribe(changed);
  }, [proof, observations]);
  // Fabric dialing begins the connection; with the saved gateway it begins when its feed is made.
  useEffect(() => {
    if (proof || fabricSnapshot.phase !== 'dialing') return;
    const held = timers.current;
    if (held.connect !== null) observations.cancel(held.connect);
    held.connect = observations.begin('ios-connect');
  }, [proof, observations, fabricSnapshot.phase]);
  const connectUrl = selected.baseUrl ?? '';
  // What the phone measured goes to the member it is connected to, as one report at a time and never
  // faster than every thirty seconds: a retry that failed waits for the next measurement to ask again.
  const reporting = useRef<{ busy: boolean; last: number; send: () => void }>({ busy: false, last: 0, send: () => {} });
  const client = useMemo(() => connectUrl ? new St3Client({ baseUrl: connectUrl, credential: () => clientCredential ?? undefined, fetchImpl: gatewayFetch(), client: clientName('smalltalk-ios', app.expo.version, process.env.EXPO_PUBLIC_ST3_BUILD) }) : null, [connectUrl, clientCredential]);
  // Image bytes go up through Expo's fetch: React Native's cannot send a byte array as a body.
  useEffect(() => {
    if (proof || !client) { observations.ready = undefined; return; }
    const state = reporting.current;
    state.send = () => {
      if (state.busy || Date.now() - state.last < 30_000) return;
      // Preparing a batch can synchronously notify ready while it closes a minute.
      state.busy = true;
      try {
        const batch = observations.nextReport();
        if (!batch) { state.busy = false; return; }
        const report = toObservationReport(batch);
        if (!report.samples.length) { observations.accepted(batch.report_id); state.busy = false; return; }
        state.last = Date.now();
        void client.observationsReport(report as never).then(
          () => observations.accepted(batch.report_id),
          error => {
            // A report the member refuses for its content is refused the same way every time: drop it, or nothing
            // this phone measures is ever reported again. Anything else (no link, an older member, a busy one)
            // sends the identical batch again at the next measurement.
            if (error instanceof ClientError && [400, 413, 422].includes(error.status)) observations.refused(batch.report_id);
          },
        ).finally(() => { state.busy = false; });
      } catch (error) {
        state.busy = false;
        throw error;
      }
    };
    observations.ready = state.send;
    return () => { if (observations.ready === state.send) observations.ready = undefined; };
  }, [proof, client, observations]);
  const uploader = useMemo(() => connectUrl ? new St3Client({ baseUrl: connectUrl, credential: () => clientCredential ?? undefined, fetchImpl: gatewayFetch(expoFetch as unknown as typeof fetch), client: clientName('smalltalk-ios', app.expo.version, process.env.EXPO_PUBLIC_ST3_BUILD) }) : null, [connectUrl, clientCredential]);

  // Settings: read once. A build that carries a fabric target (EXPO_PUBLIC_ST3_FABRIC_DEFAULT, set in an
  // ignored env file on the build host) opens on fabric until the person chooses otherwise.
  useEffect(() => {
    if (proof) return;
    let live = true;
    void (async () => {
      const [available, choice, stored, storedList, fallback] = await Promise.all([
        fabricAvailable(),
        AsyncStorage.getItem(CARRIER_KEY).catch(() => null), AsyncStorage.getItem(FABRIC_TARGET_KEY).catch(() => null),
        AsyncStorage.getItem(FABRIC_MEMBERS_KEY).catch(() => null), AsyncStorage.getItem(FABRIC_FALLBACK_KEY).catch(() => null),
      ]);
      if (!live) return;
      const built = buildFabricDefault();
      setFabricBuilt(available);
      const saved = readFabricMembers(storedList, stored);
      setFabricMemberList(saved.length ? saved : builtFabricMembers(built));
      setCarrierChoice(choice === 'fabric' || choice === 'tailscale' ? choice : available && built ? 'fabric' : 'tailscale');
      setFallbackOn(fallback !== '0');
    })();
    return () => { live = false; };
  }, [proof]);

  // Each member paired on its own keeps its profile in its own Keychain entry.
  useEffect(() => {
    if (proof) return;
    let live = true;
    const own = fabricMemberList.filter(member => !member.device);
    void Promise.all(own.map(async member => {
      const key = memberProfileKey(member);
      try { const raw = await SecureStore.getItemAsync(key); return raw ? [key, JSON.parse(raw) as PhoneProfile] as const : null; } catch { return null; }
    })).then(found => { if (live) setOwnProfiles(new Map(found.filter(entry => entry !== null))); });
    return () => { live = false; };
  }, [proof, fabricMemberList]);

  useEffect(() => { if (proof) return; Promise.allSettled([AsyncStorage.getItem(URL_KEY), AsyncStorage.getItem(ORDER_KEY), SecureStore.getItemAsync(CREDENTIAL_KEY), AsyncStorage.getItem(PROJECTION_CACHE_KEY), SecureStore.getItemAsync(PROFILE_KEY)]).then(([u, o, c, p, saved]) => {
    let paired: PhoneProfile | undefined;
    if (saved.status === 'fulfilled' && saved.value) {
      try { paired = JSON.parse(saved.value); } catch { setError('The saved device profile could not be read.'); return; }
    }
    if (saved.status === 'rejected') { setError('Secure credential storage is unavailable on this build.'); return; }
    const gateway = paired?.url ?? (u.status === 'fulfilled' ? u.value : null);
    const bearer = paired?.credential ?? (c.status === 'fulfilled' ? c.value : null);
    if (gateway) { setUrl(gateway); setUrlDraft(gateway); }
    // Only the current four-tab order is read; old phone preferences are retired.
    if (o.status === 'fulfilled' && o.value) { try { setOrder(tabOrder(JSON.parse(o.value))); } catch { /* use default */ } }
    if (bearer) {
      if (gateway && p.status === 'fulfilled') {
        const cache = hydrateProjectionForPairedDevice(p.value, gateway, true);
        if (cache) {
          setData(cache.data); setGlasses(cache.workspace.glasses); setArrangements(cache.workspace.arrangements);
          setMissionsLoaded(cache.data.missions.length > 0); setTruncated(Object.fromEntries(cache.truncated.map(key => [key, true])));
          setHasSynced(true); setStatus('connecting'); setCachedHostId(cache.hostId);
          cachedActor.current = cache.actor; cacheSavedAt.current = cache.savedAt;
        }
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
    sidebarSeen.current.clear(); sidebarCreation.current = null; sidebarRemoved.current = false;
    cachedActor.current = ''; cacheSavedAt.current = 0;
    conversationCache.current.clear(); draftCache.current.clear(); missionDetailCache.current.clear();
    draftsOwner.current = '';
    if (!proofRef.current) void localDrafts.clear();
    setGlasses([]); setGlassesIssue(''); setArrangements([]); setArrangementsIssue(''); setSidebarTerminals([]); setSidebarResources([]); setSidebarResourcesIssue(''); sidebarResourcesRead.current = null;
    setData(emptyData); setTruncated({}); setHasSynced(false); setCachedHostId(''); setSnapshot(null);
  }
  async function clearCachedProjection() { cacheGeneration.current++; clearCaches(); if (!proof) await AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {}); }

  // Capabilities once per connection: limits, the session's actor, and what it may control.
  const loadCapabilities = useCallback(async (current: () => boolean) => {
    if (!client) return;
    try {
      const capability = await client.capabilities();
      if (!current()) return;
      if (cachedActor.current && cachedActor.current !== capability.value.session_actor) { clearCaches(); if (!proofRef.current) void AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {}); }
      cachedActor.current = capability.value.session_actor;
      setCaps(capability.value);
    } catch (e) { if (current()) setError(errorText(e)); }
  }, [client]);

  useEffect(() => {
    setSync(undefined); setSyncStale(false);
    if (!client || status !== 'online' || proofRef.current) return;
    return watchSync(async () => (await client.attentionList({ limit: 1 })).value.sync ?? null, (value, stale) => { setSync(value); setSyncStale(stale); });
  }, [client, status]);

  // One collections socket per paired gateway and credential keeps attention, missions, and agents
  // current. It closes in the background and opens fresh, snapshots first, in the foreground.
  useEffect(() => {
    if (!client || !credential) { if (!credential) setStatus('setup'); return; }
    const generation = cacheGeneration.current;
    let active = true;
    let connection = 0;
    const current = () => active && generation === cacheGeneration.current;
    attentionLive.current = false;
    if (!proofRef.current && timers.current.connect === null) timers.current.connect = observations.begin('ios-connect');
    const opened = new Feed(client, {
      onWindow: (name, rows, hasMore, at, received) => {
        if (!current()) return;
        if (at?.id) lastSnapshot.current = at.id;
        if (!proofRef.current) {
          if (name === 'attention') { quality.received(1, received); quality.attentionWindow(rows as Attention[], hasMore); }
          if (name === 'agents') quality.received(2, received);
        }
        // Home decides what of attention to show, as stui does; agents drop only history.
        const shown = name === 'agents' ? (rows as Array<{ operational?: { layer?: string } }>).filter(currentAgent) : rows;
        // Nothing leaves Home by itself: what st closes while it is shown stays until cleared.
        const seen = attentionLive.current;
        if (name === 'attention') attentionLive.current = true;
        setData(previous => ({ ...previous, [name]: name === 'attention' && seen ? keepClosed(previous.attention, shown as Attention[], acted.current, capsRef.current?.session_actor) : shown }));
        setTruncated(previous => ({ ...previous, [name]: hasMore }));
        setLoadErrors(previous => { if (!(name in previous)) return previous; const rest = { ...previous }; delete rest[name]; return rest; });
        if (name === 'missions') setMissionsLoaded(true);
        proofRef.current?.record('window', { name, rows: shown.length });
        setSnapshot(at); setCachedHostId(at.host_id); setHasSynced(true);
      },
      onConnection: (state, issue) => {
        if (!current()) return;
        proofRef.current?.record('feed', { state });
        if (!proofRef.current) { quality.live(state === 'live'); if (state !== 'live') quality.resetWindows(); }
        setStatus(state === 'live' ? 'online' : state === 'connecting' ? 'connecting' : 'offline');
        setConnectionIssue(issue ?? '');
        if (!proofRef.current) {
          const held = timers.current;
          if (state === 'live') {
            observations.setLive(true);
            for (const key of ['openToLive', 'connect', 'recover'] as const) { const token = held[key]; if (token !== null) { observations.end(token); held[key] = null; } }
            held.everLive = true;
            // The path a fabric connection took is known once it carries traffic: read it once, now.
            if (routeRef.current.kind === 'fabric') void fabricPathInUse().then(path => setFabricPath(path));
          } else {
            observations.setLive(false);
            // A dropped connection that had been live starts a recovery, timed until it is live again.
            if (state === 'reconnecting' && held.everLive && held.recover === null) held.recover = observations.begin('ios-recover');
          }
        }
        if (state !== 'live') { connection++; setCaps(null); }
        if (state === 'live') {
          const epoch = connection;
          setError(''); void loadCapabilities(() => current() && epoch === connection);
          carrier.feedLive(); feedMisses.current = 0;
        }
        // The bridge's own connection failing twice running (one dropped socket the feed mends itself):
        // a refusal ends the trial, anything else is redialed.
        else if (state === 'reconnecting' && routeRef.current.kind === 'fabric' && ++feedMisses.current >= 2) void carrier.feedFailed();
      },
      onWindowError: (name, message) => { if (current()) setLoadErrors(previous => ({ ...previous, [name]: message })); },
      // Releasing a subscription keeps its last rows for navigation and offline use.
      // The next focused mission screen follows again and replaces them with a snapshot.
      onConversationFrame: (rows, replace) => { if (current()) proofRef.current?.record('conversation', { rows, replace }); },
      onTiming: (target, ms) => { if (current() && !proofRef.current) observations.observe(target, ms); },
    }, foreground.current, actionId, undefined, undefined, false, undefined, nativeCollectionSocket);
    // Paired: connecting from here on, even while the app waits to be active before it dials.
    setStatus(previous => previous === 'setup' ? 'connecting' : previous);
    setFeed(opened);
    return () => { active = false; opened.close(); setFeed(held => held === opened ? null : held); };
  }, [client, credential, loadCapabilities, observations, carrier]);
  // No client yet: either nothing is paired, fabric is opening, or neither route can be used.
  useEffect(() => {
    if (client || proof) return;
    if (!credential || (!url && !fabricTarget)) setStatus('setup');
    else if (selected.pending) setStatus('connecting');
    else { setStatus('offline'); setConnectionIssue(selected.issue || (isFabricGatewayId(url) ? 'This device is paired over Fabric only. Turn Fabric on to connect.' : '')); }
  }, [client, proof, credential, url, fabricTarget, selected.pending, selected.issue]);
  useEffect(() => {
    if (proof) foreground.current.update(proof.ready && url === proof.url ? AppState.currentState : 'background');
  }, [url, proof?.url, proof?.ready]);
  useEffect(() => {
    const subscription = AppState.addEventListener('change', state => foreground.current.update(proofRef.current && !proofRef.current.ready ? 'background' : state));
    return () => subscription.remove();
  }, []);

  const inventoryGranted = caps === null ? undefined : caps.capabilities.some(capability => capability.id === 'inventory-collections' && capability.version >= 1 && capability.state === 'granted');
  useEffect(() => { feed?.setInventoryGranted(inventoryGranted === true); }, [feed, inventoryGranted]);

  // Spaces: followed on the feed's socket when the gateway grants them.
  useEffect(() => { void AsyncStorage.getItem(SIMPLE_KEY).then(value => setSimpleOn(value !== '0')).catch(() => {}); }, []);
  const arrangementPerson = caps?.session_actor.match(/^person\/[^/]+/)?.[0];
  const arrangementsGranted = caps?.capabilities.some(capability => capability.id === 'arrangements' && capability.version >= 1 && capability.state === 'granted') ?? false;
  useEffect(() => {
    if (!feed || !arrangementsGranted || !arrangementPerson) return;
    const follow = feed.followArrangements(arrangementPerson, { onArrangements: setArrangements, onIssue: setArrangementsIssue });
    return () => follow.close();
  }, [feed, arrangementPerson, arrangementsGranted]);
  const loadSidebarTerminals = useCallback(async () => {
    if (!client) return;
    if (inventoryGranted !== false) return;
    const generation = cacheGeneration.current;
    try {
      const result = await listCollectionPages(options => client.terminalsList({ ...options, history: false }), 200);
      if (generation !== cacheGeneration.current) return;
      setSidebarTerminals(result.pages.flatMap(page => page.value.items.filter((item): item is Runtime => item.kind === 'runtime' && !!item.terminal_id)));
      setSidebarTerminalsIssue(result.truncated ? 'More terminals exist beyond this window.' : '');
    } catch (error) { if (generation === cacheGeneration.current) setSidebarTerminalsIssue(errorText(error)); }
  }, [client, inventoryGranted]);
  const watchSidebarTerminals = useCallback(() => {
    if (!feed || inventoryGranted !== true) return () => {};
    return feed.watchInventory('terminals', {
      onRows: (rows, hasMore) => { setSidebarTerminals(rows.filter(row => !!row.terminal_id)); setSidebarTerminalsIssue(hasMore ? 'More terminals exist beyond this window.' : ''); },
      onIssue: setSidebarTerminalsIssue,
    });
  }, [feed, inventoryGranted]);
  const loadSidebarResources = useCallback((focused?: string): Promise<void> => {
    if (!client || status !== 'online') return Promise.resolve();
    const generation = cacheGeneration.current;
    if (sidebarResourcesRead.current) {
      if (!focused || focused === sidebarResourcesRead.current.focused) return sidebarResourcesRead.current.promise;
      return sidebarResourcesRead.current.promise.then(() => { if (generation === cacheGeneration.current) return loadSidebarResources(focused); });
    }
    const pending = readSidebarResources((filters, options) => client.resourcesList(filters, options), focused).then(result => {
      if (generation !== cacheGeneration.current) return;
      setSidebarResources(result.rows); setSidebarResourcesIssue(result.truncated ? 'More graph resources exist beyond this window.' : '');
    }).catch(error => { if (generation === cacheGeneration.current) setSidebarResourcesIssue(`Resources not available on this connection: ${errorText(error)}`); })
      .finally(() => { if (sidebarResourcesRead.current?.promise === pending) sidebarResourcesRead.current = null; });
    sidebarResourcesRead.current = { focused, promise: pending };
    return pending;
  }, [client, status]);
  // Version 1 glasses are splits of tab groups; an earlier member's glasses are a shape this app no longer reads.
  const glassesGranted = caps?.capabilities.some(capability => capability.id === 'glasses' && capability.version >= 1 && capability.state === 'granted') ?? false;
  useEffect(() => {
    if (!feed || !glassesGranted) return;
    const follow = feed.followGlasses({ onGlasses: setGlasses, onIssue: setGlassesIssue });
    return () => follow.close();
  }, [feed, glassesGranted]);
  // Keep the last data for an offline start, saved at most every 10 s while it changes.
  useEffect(() => {
    if (proof || !hasSynced || !snapshot || !caps) return;
    const timer = setTimeout(() => {
      cacheSavedAt.current = Date.now();
      const truncatedKeys = (Object.keys(truncated) as Array<keyof Data>).filter(key => truncated[key]);
      const encoded = encodeProjectionCache(url, caps.session_actor, snapshot.host_id, snapshot.store_index, { ...data, attention: data.attention.filter(item => !item.closedElsewhere) }, cacheSavedAt.current, truncatedKeys, { glasses, arrangements });
      if (encoded) void AsyncStorage.setItem(PROJECTION_CACHE_KEY, encoded).catch(() => { cacheSavedAt.current = 0; });
    }, Math.max(0, cacheSavedAt.current + 10_000 - Date.now()));
    return () => clearTimeout(timer);
  }, [caps, data, glasses, arrangements, hasSynced, snapshot, truncated, url]);

  // Launches and older daemons retain their HTTP reads. Granted inventory windows carry
  // action updates themselves, so an action does not reread them.
  const loadLists = useCallback(async (keys: readonly OnDemand[]) => {
    if (!client || !keys.length) return;
    keys = keys.filter(key => key === 'launches' || inventoryGranted === false);
    if (!keys.length) return;
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
  }, [client, inventoryGranted]);

  const watchLists = useCallback((keys: readonly OnDemand[]) => {
    if (inventoryGranted !== true || !feed) return () => {};
    const stops = keys.filter(key => key !== 'launches').map(key => feed.watchInventory(key as 'machines' | 'devices' | 'sessions', {
      onRows: (rows, hasMore, at) => {
        setData(previous => ({ ...previous, [key]: rows }));
        setTruncated(previous => ({ ...previous, [key]: hasMore }));
        setSnapshot(at);
      },
      onIssue: issue => setLoadErrors(previous => { const next = { ...previous }; if (issue) next[key] = issue; else delete next[key]; return next; }),
    }));
    return () => { for (const stop of stops) stop(); };
  }, [feed, inventoryGranted]);

  /** A fence on a snapshot read just now: one read seconds ago has nearly always moved on a busy host. */
  async function fence(revisions: Record<string, string> = {}) {
    if (!client) throw new Error('Still connecting; try again in a moment.');
    const capability = await client.capabilities();
    return { snapshot_id: capability.snapshot.id, subject_revisions: revisions };
  }
  /**
   * Run an action, trying again (with a fresh fence each time) while st refused it only because it
   * raced a busy store, as `retryable` says.
   */
  async function runAction(action: () => Promise<unknown>, reload: readonly OnDemand[] = [], retryable: (error: unknown) => boolean = notApplied): Promise<boolean> {
    if (status !== 'online' || (proof && !proof.ready)) { setError('Still connecting; try again in a moment.'); return false; }
    setBusy(true);
    try { await retryTransient(8, action, retryable); setError(''); await loadLists(reload); return true; } catch (e) { setError(errorText(e)); return false; } finally { setBusy(false); }
  }

  /**
   * Pair this device with its member, or, with `member`, pair one fabric member on its own: that
   * member's profile is its own Keychain entry, and this device's own pairing, cache and gateway
   * are left as they are.
   */
  async function completePairing(id: string, code: string, fingerprint: string, unpinned: boolean, gateway: string, member?: FabricMember) {
    if (proof) throw new Error('Close the fabric proof to change ordinary device pairing.');
    const pin = validatePairingTrust(fingerprint, unpinned);
    if (!canVerifyPairing()) throw new Error('This build cannot verify pairing proofs; update the phone app.');
    const profileKey = member ? memberProfileKey(member) : PROFILE_KEY;
    if (member && !(fabricSnapshot.phase === 'ready' && fabricSnapshot.member === member.name)) throw new Error(`Fabric is not connected to ${member.name} yet. Wait a moment and try again, or check ${member.name}'s fabric grant for this phone.`);
    const oldRaw = await SecureStore.getItemAsync(profileKey);
    const old: PhoneProfile | undefined = oldRaw ? JSON.parse(oldRaw) : undefined;
    // Anonymous discovery reveals no key material and never receives an old bearer.
    const requestFetch = gatewayFetch();
    // With fabric chosen and its bridge open, the pairing requests go over fabric to the same member: the
    // gateway typed (or saved) stays this device's identity and its fallback route.
    const requestUrl = member ? fabricSnapshot.bridgeUrl : fabricBuilt && carrierChoice === 'fabric' && fabricSnapshot.phase === 'ready' ? fabricSnapshot.bridgeUrl : gateway;
    if (isFabricGatewayId(gateway) && requestUrl === gateway) throw new Error('Fabric is not connected yet, so there is nowhere to pair. Wait a moment and try again, or check the member\'s fabric grant for this phone.');
    const response = await requestFetch(`${requestUrl}/v1/client/capabilities`);
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
      const gatewayClient = new St3Client({ baseUrl: requestUrl, fetchImpl: requestFetch, client: clientName('smalltalk-ios', app.expo.version, process.env.EXPO_PUBLIC_ST3_BUILD) });
      const result = await gatewayClient.completePairing(id, { api_version: API_VERSION, code, device_public_key: made.key, key_storage: made.storage });
      consumed = true; deviceId = result.value.device_id;
      if (result.value.kind !== 'paired-session') throw new Error('A phone profile cannot enroll an external assistant.');
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
      await SecureStore.setItemAsync(profileKey, JSON.stringify(next), { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY });
      if (!signs) await removeDeviceKey(made.handle).catch(() => {});
      if (old?.signing?.handle) await removeDeviceKey(old.signing.handle).catch(() => {});
      if (member) {
        setOwnProfiles(held => new Map(held).set(profileKey, next));
        setPairingMember(null); setError('');
        return;
      }
      if (!oldRaw) await removeDeviceKey().catch(() => {});
      await SecureStore.deleteItemAsync(CREDENTIAL_KEY).catch(() => {});
      await SecureStore.deleteItemAsync(SIGNING_KEY).catch(() => {});
      await clearCachedProjection().catch(() => {});
      setUrl(gateway); setUrlDraft(gateway); setCredential(next.credential); setPairDraft(null); setPairingIssue(''); setError('');
      // Paired over fabric alone: that member uses this device's own pairing.
      if (isFabricGatewayId(gateway) && fabricTarget && fabricGatewayId(fabricTarget) === gateway && !fabricTarget.device) {
        await saveFabricMembers(fabricMemberList.map(m => (sameFabricTarget(m, fabricTarget) ? { ...m, device: true as const } : m)));
      }
    } catch (error) {
      await removeDeviceKey(made.handle).catch(() => {});
      if (consumed) {
        const safeId = /^device\/[0-9a-f]{24}$/.test(deviceId) ? deviceId : 'the new device';
        throw new Error(`${errorText(error)} The code was consumed; inspect and revoke ${safeId} on the trusted member before pairing again. The previous profile was retained.`);
      }
      throw error;
    }
  }

  /** Forget this phone's own pairing with one member: its Keychain entry and its signing key. */
  async function forgetMemberProfile(member: FabricMember): Promise<void> {
    const key = memberProfileKey(member);
    const profile = ownProfiles.get(key);
    await SecureStore.deleteItemAsync(key).catch(() => {});
    if (profile?.signing?.handle) await removeDeviceKey(profile.signing.handle).catch(() => {});
    setOwnProfiles(held => { const next = new Map(held); next.delete(key); return next; });
  }
  /** Save the fabric members in the person's order: the list under its key, the device's member under the old key. */
  async function saveFabricMembers(next: FabricMember[]): Promise<void> {
    const list = fabricMembers(next);
    const stored = storedFabricMembers(list);
    await Promise.all([
      stored.members ? AsyncStorage.setItem(FABRIC_MEMBERS_KEY, stored.members) : AsyncStorage.removeItem(FABRIC_MEMBERS_KEY),
      stored.target ? AsyncStorage.setItem(FABRIC_TARGET_KEY, stored.target) : AsyncStorage.removeItem(FABRIC_TARGET_KEY),
    ].map(write => write.catch(() => {})));
    setFabricMemberList(list);
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
      // With fabric chosen and a target saved, a device with no gateway pairs over fabric alone.
      const fabricOnly = !pairDraft && !url && fabricBuilt && carrierChoice === 'fabric' && fabricTarget ? fabricGatewayId(fabricTarget) : '';
      const gateway = pairDraft?.gateway ?? (url || fabricOnly);
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
    /** Choose the carrier. Fabric is an opt-in and needs a saved target; the saved gateway stays as it is. */
    async setCarrier(choice: CarrierChoice) { await AsyncStorage.setItem(CARRIER_KEY, choice).catch(() => {}); setCarrierChoice(choice); },
    /** Add a member to the end of the fabric list from pasted text (`node=…&service=…`, or a link carrying them) and a name. */
    async saveFabricTarget(text: string, name = ''): Promise<boolean> {
      const target = fabricTargetFromText(text);
      if (!target) { setError('That is not a fabric target. It needs node=NODE_ID (64 hex digits) and service=NAME.'); return false; }
      if (fabricMemberList.some(existing => sameFabricTarget(existing, target))) { setError('That member is already in the list.'); return false; }
      if (fabricMemberList.length >= MAX_FABRIC_MEMBERS) { setError(`The list holds at most ${MAX_FABRIC_MEMBERS} members. Remove one first.`); return false; }
      const named = memberName(name);
      if (named && fabricMemberList.some(existing => sameName(existing.name, named))) { setError(`A member is already named ${named}.`); return false; }
      // The first member of a phone paired over a gateway is the member it is paired with, as with today's
      // single target. A phone paired over fabric alone already knows its member. Any other member is
      // paired on its own: this device's credential never goes to it.
      const device = !fabricMemberList.some(m => m.device) && (isFabricGatewayId(url) ? url === fabricGatewayId(target) : !fabricMemberList.length);
      await saveFabricMembers([...fabricMemberList, { ...target, name: named || defaultMemberName(fabricMemberList.length), ...(device ? { device: true as const } : {}) }]);
      setError('');
      return true;
    },
    /** Remove one member from the fabric list, and its own pairing with it. */
    async removeFabricMember(index: number) {
      const member = fabricMemberList[index];
      if (!member) return;
      if (!member.device) await forgetMemberProfile(member);
      if (pairingMember && sameFabricTarget(pairingMember, member)) setPairingMember(null);
      await saveFabricMembers(fabricMemberList.filter((_, i) => i !== index));
    },
    /** Open the form to pair this phone with one member on its own; fabric connects to that member alone meanwhile. */
    startMemberPairing(index: number) {
      const member = fabricMemberList[index];
      if (!member || member.device) return;
      setPairingMember(member); setError('');
      carrier.pick(member);
    },
    cancelMemberPairing() { setPairingMember(null); carrier.retry(); },
    /** Pair this phone with the member being paired, from the pairing ID, code and fingerprint its member shows. */
    async pairMember(id: string, code: string, fingerprint: string): Promise<boolean> {
      const member = pairingMember;
      if (!member || !id.trim() || !code.trim()) return false;
      setBusy(true);
      try { await completePairing(id.trim(), code.trim(), fingerprint, false, fabricGatewayId(member), member); return true; } catch (e) { setError(errorText(e)); return false; } finally { setBusy(false); }
    },
    /** Move a member up (-1) or down (+1) the list: the walk follows the person's order. */
    async moveFabricMember(index: number, by: -1 | 1) {
      const to = index + by;
      if (index < 0 || to < 0 || index >= fabricMemberList.length || to >= fabricMemberList.length) return;
      const next = fabricMemberList.slice();
      [next[index], next[to]] = [next[to], next[index]];
      await saveFabricMembers(next);
    },
    /** Rename a member; an empty name keeps the one it has. */
    async renameFabricMember(index: number, name: string) {
      const named = memberName(name);
      if (!named || !fabricMemberList[index]) return;
      if (fabricMemberList.some((m, i) => i !== index && sameName(m.name, named))) { setError(`A member is already named ${named}.`); return; }
      await saveFabricMembers(fabricMemberList.map((m, i) => (i === index ? { ...m, name: named } : m)));
    },
    /** Use one member by hand: only it is dialed, also after another member refused. A member not paired yet is paired first. */
    pickFabricMember(index: number) {
      const member = fabricMemberList[index];
      if (member && carrierMembers.some(m => sameFabricTarget(m, member))) carrier.pick(member);
    },
    /** Whether a fabric that cannot be used falls back to the saved gateway. */
    async setFabricFallback(on: boolean) { await AsyncStorage.setItem(FABRIC_FALLBACK_KEY, on ? '1' : '0').catch(() => {}); setFallbackOn(on); },
    retryFabric() { carrier.retry(); },
    /** This phone's public fabric node ID, for the member's grant; created on first use. */
    async loadFabricNode() { if (!fabricBuilt) return; try { setFabricNode(await fabricIdentity()); } catch { setFabricNode(''); } },
    /** The path the live fabric connection uses, read on demand. */
    async refreshFabricPath() { setFabricPath(await fabricPathInUse()); },
    async forget() {
      if (proof) { proof.close(); return; }
      const saved = await SecureStore.getItemAsync(PROFILE_KEY);
      const profile: PhoneProfile | undefined = saved ? JSON.parse(saved) : undefined;
      await SecureStore.deleteItemAsync(PROFILE_KEY);
      if (profile?.signing?.handle) await removeDeviceKey(profile.signing.handle);
      await SecureStore.deleteItemAsync(CREDENTIAL_KEY); await SecureStore.deleteItemAsync(SIGNING_KEY); await removeDeviceKey(); await clearCachedProjection();
      for (const member of fabricMemberList) if (!member.device) await forgetMemberProfile(member);
      setCredential(null); setCaps(null); setPairDraft(null); setPairingIssue(profile?.personRootFingerprint ? '' : REPAIR_WARNING); setHistoricalSessions([]); setStatus('setup');
    },
    /** Complete a person step; `answer` is a structured request's named answer, by id. */
    async editSidebar(operations: ArrangementOperation[]): Promise<boolean> {
      if (!client || status !== 'online' || !arrangementPerson || !arrangementsGranted || sidebarWriting.current) return false;
      const existing = sidebarArrangement(arrangements);
      // A retired Sidebar is never created again.
      if (!existing && sidebarRemoved.current) { setError(SIDEBAR_RETIRED); return false; }
      const draft = sidebarCreation.current ?? { subject: sidebarSubject(arrangementPerson), created: false };
      if (!existing) sidebarCreation.current = draft;
      const creating = !existing && !draft.created;
      const parameters = { subject: existing?.id ?? draft.subject, owner: arrangementPerson, operations: creating ? [{ op: 'create' as const, name: 'Sidebar' }, ...operations] : operations };
      const id = actionId();
      sidebarWriting.current = true;
      try {
        const done = await runAction(async () => {
          try { return await client.arrangementEdit({ id, idempotency_key: id, fence: await fence(), parameters }); } catch (e) { if (creating && sidebarRetired(e)) sidebarRemoved.current = true; throw e; }
        });
        if (done && creating) draft.created = true;
        if (!done && sidebarRemoved.current) setError(SIDEBAR_RETIRED);
        return done;
      } finally { sidebarWriting.current = false; }
    },
    async done(item: Attention, summary: string, answer?: string) {
      if (!client) return false;
      acted.current.add(item.id);
      const typed = personAnswer(item.request, answer, summary);
      if (typeof typed === 'string') { setError(typed); return false; }
      if (status !== 'online' || (proof && !proof.ready)) { setError('Still connecting; try again in a moment.'); return false; }
      setBusy(true);
      const qualityToken = proof ? 0 : quality.answerSent(item.id);
      let qualityAccepted = false;
      try {
        // An answer that landed is an answer, however the reply came back (Cos, 2026-10-10: the app
        // said it failed for a decision that was recorded, and Nathan answered it again).
        const outcome = await submitAnswer({
          build: async () => { const id = actionId(); return { id, idempotency_key: id, fence: await fence({ [item.id]: item.revision }), parameters: { target_id: item.source_id, episode: item.episode || item.revision, summary, ...(typed ? { answer: typed } : {}) } }; },
          send: request => client.workDone(request),
          stillWaiting: async () => {
            try { const current = (await client.attentionGet(item.id)).value; return current.kind !== 'attention' || current.state === 'open'; }
            catch (e) { if (e instanceof ClientError && e.response.code === 'not-found') return false; throw e; }
          },
          wait: ms => new Promise(resolve => setTimeout(resolve, ms)),
        });
        if (!outcome.ok) { setError(errorText(outcome.error)); return false; }
        qualityAccepted = true;
        setError('');
        await loadLists([]);
        return true;
      } finally { if (qualityToken) quality.answerConfirmed(item.id, qualityToken, qualityAccepted); setBusy(false); }
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
    /**
     * Answer a native prompt from its alert with one of the answers it offers: a question's
     * `answer` sends `sent`, its choices (see `questionAnswer`). Only the prompt still waiting is
     * answered, once; an answer in the terminal wins, and a prompt st says is gone is not sent again.
     */
    async respondPrompt(item: Attention, answer: string, sent: string = answer) {
      if (!client) return false;
      const prompt = promptAnswers(item);
      if (!prompt || !prompt.answers.includes(answer)) { setError('This prompt cannot be answered here. Answer it in the terminal.'); return false; }
      acted.current.add(item.id);
      const qualityToken = proof ? 0 : quality.answerSent(item.id);
      const accepted = await runAction(async () => { const id = actionId(); return client.promptRespond({ id, idempotency_key: id, fence: await fence({ [item.id]: item.revision }), parameters: { target_id: prompt.target_id, episode: prompt.episode, answer: sent } }); }, [], error => notApplied(error) && !promptGone(error));
      if (qualityToken) quality.answerConfirmed(item.id, qualityToken, accepted);
      return accepted;
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
    /** The whole text of a long message (a conversation shows only its start), read from the member that took it. */
    async messageBody(message: string): Promise<{ text: string; complete?: boolean }> {
      if (!client || status !== 'online') throw new Error(status === 'online' ? 'not connected' : 'offline');
      const answer = (await client.messageBodyGet(message)).value;
      return { text: answer.text, complete: answer.complete };
    },
    /** Send Small Talk to an agent, as stui does: fenced to a fresh snapshot, once more if it moved. */
    async send(to: string, content: string, sessionId?: string, tags?: string[], images: Picked[] = []): Promise<Sent> {
      if (!client || !uploader) return { how: 'failed', error: 'not connected' };
      if (status !== 'online' || (proof && !proof.ready)) return { how: 'failed', error: 'offline' };
      // Images are kept on this member first; the message names them, and st fetches each from
      // here for a reader on another machine (docs/st3/attachments.md).
      const attachments: AttachmentInput[] = [];
      for (const image of images) {
        try {
          const kept = (await uploader.uploadBlob(decodeBase64(image.base64), image.mediaType)).value;
          attachments.push({ blob: kept.blob, media_type: image.mediaType, ...(image.name ? { name: image.name } : {}) });
        } catch (e) {
          if (e instanceof ClientError && e.status === 404) return { how: 'failed', error: 'the st this phone is paired with cannot carry images yet; it needs a newer st' };
          return { how: 'failed', error: `the image could not be sent: ${errorText(e)}` };
        }
      }
      const began = performance.now();
      // The first try goes on the snapshot a window frame already brought, with no capabilities read;
      // each try after st said the last applied nothing reads a fresh one.
      const fences = sendFences(lastSnapshot.current, fence);
      const outcome = await sendMessage<MessageRequest>({
        build: async () => {
          const id = actionId();
          const unsigned = { to, content, ...(sessionId ? { session_id: sessionId } : {}), ...(tags?.length ? { tags } : {}), ...(attachments.length ? { attachments } : {}) };
          const parameters = proof ? unsigned : await signMessage(id, unsigned, signingKeyRef.current);
          return { id, idempotency_key: id, fence: await fences(), parameters };
        },
        send: request => client.messageSend(request),
        wait: ms => new Promise(resolve => setTimeout(resolve, ms)),
      });
      if (outcome.how !== 'sent') { proof?.record(outcome.how === 'unconfirmed' ? 'message-unconfirmed' : 'message-failed', { elapsedMs: performance.now() - began }); return sentAs(outcome); }
      proof?.record('message-accepted', { elapsedMs: performance.now() - began });
      // The person's wait, fence read and any repeat included, to st's acknowledgement.
      if (!proof) observations.observe('ios-message-ack', performance.now() - began);
      return outcome;
    },
    /** Send an unconfirmed message again as the same request (id, key, signature and nonce), freshly fenced. */
    async resend(request: MessageRequest): Promise<Sent> {
      // Offline it stays as it was, ready to send again.
      if (!client || status !== 'online' || (proof && !proof.ready)) return { how: 'unconfirmed', request };
      return sentAs(await resendMessage<MessageRequest>({
        fence: sendFences(lastSnapshot.current, fence),
        send: next => client.messageSend(next),
        wait: ms => new Promise(resolve => setTimeout(resolve, ms)),
      }, request));
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

  };

  const knownHostId = snapshot?.host_id ?? cachedHostId;
  const gatewayMachineId = knownHostId ? `machine/${knownHostId.replace(/^host\//, '')}` : '';
  const gatewayHost = data.machines.find(m => m.id === gatewayMachineId)?.name ?? (knownHostId ? knownHostId.replace(/^host\//, '') : '');
  const canControlTerminal = caps?.capabilities.some(capability => capability.id === 'terminal.input' && capability.state === 'granted') ?? false;

  return {
    actor: caps?.session_actor ?? cachedActor.current, cachedAt: cacheSavedAt.current, order, url, urlDraft, setUrlDraft, credential, data, truncated, loadErrors, feed, connectionIssue, caps, snapshot, status, hasSynced, sync, syncStale, missionsLoaded,
    error, setError, pairingIssue, setPairingIssue, pairDraft, busy, historicalSessions, conversationCache, draftCache, draftsReady, client,
    gatewayMachineId, gatewayHost, canControlTerminal, loadLists, watchLists, inventoryGranted, actions,
    saveDrafts: () => { if (!proof) void localDrafts.save(draftsOwner.current, draftCache.current); },
    treeView, setTreeView, scrollRequest, requestScroll: (y: number) => setScrollRequest({ y, at: Date.now() }),
    glassesGranted, glasses, glassesIssue, simpleOn,
    carrierInfo: { choice: carrierChoice, built: fabricBuilt, target: fabricTarget, members: fabricMemberList, pairings: fabricMemberList.map(member => memberPairing(member, !!credential, ownKeys)), pairingMember, fallback: fallbackOn, route: selected.route, routeText: routeLabel(selected.route), pending: selected.pending, issue: selected.issue, fabric: fabricSnapshot, node: fabricNode, path: fabricPath },
    sidebarSeen, arrangements, arrangementsIssue, sidebarTerminals, sidebarTerminalsIssue, loadSidebarTerminals, watchSidebarTerminals, sidebarResources, sidebarResourcesIssue, loadSidebarResources,
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

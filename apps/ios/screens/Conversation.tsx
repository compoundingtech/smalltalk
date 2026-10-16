import { Component, createRef, memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { ActionSheetIOS, FlatList, Image, Keyboard, KeyboardAvoidingView, Platform, Pressable, StyleSheet, TextInput, View, type ViewToken } from 'react-native';
import * as ImagePicker from 'expo-image-picker';
import * as Clipboard from 'expo-clipboard';
import { useFocusEffect } from '@react-navigation/native';
import { useHeaderHeight } from '@react-navigation/elements';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import type { St3Client, TimelineEntry } from '../../../clients/typescript/st3-client';
import type { ConversationContentRef } from '../../../clients/typescript/st3-client';
import { contentImageUri, contentJsonValue, fetchedConversationEntries, loadConversationContent } from '@smalltalk/st3-views';
import { checkoutLabel } from '../launcher';
import { agentGlyph, agentModel, agentName, agentReported, agentState, agentWord, harnessColor, harnessName, loginGuidance } from '../agentsView';
import { Banners } from '../chrome';
import { quality } from '../qualityNative';
import rules from '../../../fixtures/clients/conversation-style.json';
import { tokenColor, type ConversationRules } from '../conversationStyle';
import { conversationEntries, freshnessLine, staleLine, entryMatches, entryText, folds, headerLine, shownToolLines, subagentSession, unreadableTranscript, type ConversationEntry, type MailImage, simplify, type SimpleRow, sessionPerson, applyConversation, applyOlderPage, isUnresolved, olderFailed, olderLoading, olderNote, type Conversation } from '@smalltalk/st3-views';
import { wholeText, wholeTextFailure } from '../messageBody';
import { draftSize, messageLimit, restoreDraft, sendReason } from '../composer';
import { addImages, fromDataUri, MAX_IMAGES, megabytes, picked, type Picked } from '../images';
import { rememberBounded } from '../boundedCache';
import { ConversationImageViewport, VisibleConversationImage, type VisibleImageState } from '../visibleConversationImages';
import { dictationAvailable, startDictation } from '../modules/st-dictation';
import type { RootScreen } from '../navigation';
import { openSubagentConversation } from '../conversationNavigation';
import { useStore, type MessageRequest, type Sent } from '../store';
import { fonts, theme } from '../theme';
import { Button, Field, LINE, Markdown, T } from '../ui';
import { AlertsStrip } from './Alerts';

// Drawn by stui's rules (fixtures/clients/conversation-style.json), so both apps look alike.
const RULES: ConversationRules = rules;
const c = tokenColor;

const empty: Conversation<TimelineEntry> = { entries: [], hasOlder: false, newestSequence: -1, older: { paged: false, start: false, loading: false } };
// `unconfirmed` is the exact request st may have taken and has not confirmed, to send again unchanged.
type Pending = { id: string; text: string; at: string; failed?: string; unconfirmed?: MessageRequest; resending?: boolean; images?: number };
type Row = { kind: 'entry'; entry: ConversationEntry } | Exclude<SimpleRow, { kind: 'entry' }> | { kind: 'pending'; pending: Pending } | { kind: 'older' };

function nowClock() {
  const at = new Date();
  return `${String(at.getHours()).padStart(2, '0')}:${String(at.getMinutes()).padStart(2, '0')}`;
}

// One agent's conversation, drawn as stui draws it and kept pinned to the newest entry just
// above the composer, unless the person has scrolled back to read.
export function ConversationScreen({ route, navigation }: RootScreen<'Conversation'>) {
  const { target, sessionId } = route.params;
  const { data, feed, caps, status, historicalSessions, conversationCache, draftCache, draftsReady, saveDrafts, actions, simpleOn } = useStore();
  const session = [...data.sessions, ...historicalSessions].find(candidate => candidate.id === (sessionId ?? target));
  // A running session st manages belongs to its agent: name it, and send to it, as the agent.
  const agent = data.agents.find(candidate => candidate.id === target)
    ?? (session?.state === 'running' && session.managed !== false ? data.agents.find(candidate => candidate.id === session.owner_id) : undefined);
  const unresolved = session ? isUnresolved(session) : false;
  const title = route.params.title ?? (agent ? agentName(agent) : session?.driver ?? target.split('/').pop() ?? target);
  const [timeline, setTimeline] = useState<Conversation<TimelineEntry>>(() => conversationCache.current.get(target) ?? empty);
  const timelineNow = useRef(timeline);
  timelineNow.current = timeline;
  // Earlier pages load only when the person asks by scrolling (or tapping the line above the
  // oldest entry), never on their own when a short conversation fits the screen.
  const dragged = useRef(false);
  const loadOlder = () => {
    const current = timelineNow.current;
    const sessionId = current.sessionId;
    if (!dragged.current || !current.hasOlder || current.older.loading || !sessionId || status !== 'online') return;
    const keep = (next: Conversation<TimelineEntry>) => { rememberBounded(conversationCache.current, target, next, 24); return next; };
    setTimeline(previous => olderLoading(previous));
    void actions.olderTimeline(sessionId, current.older, current.entries[0]).then(
      page => setTimeline(previous => keep(applyOlderPage(previous, sessionId, page))),
      error => setTimeline(previous => olderFailed(previous, error instanceof Error ? error.message : String(error))),
    );
  };
  // Whether st has answered at all: until it has, the screen says it is loading, never "nothing".
  const [loaded, setLoaded] = useState(() => conversationCache.current.has(target));
  const [issue, setIssue] = useState('');
  // st refused for good: nothing asks again until the person taps to retry, which follows again.
  const [stopped, setStopped] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const retry = () => { if (stopped) { setStopped(false); setIssue(''); setAttempt(count => count + 1); } };
  // When st last sent this conversation, so a view that may be stale says how old it is.
  const [lastFrame, setLastFrame] = useState<number | null>(null);
  const [now, setNow] = useState(() => Date.now());
  // Asking st again: true from the moment the phone starts following this conversation until st sends it.
  const [refreshing, setRefreshing] = useState(true);
  // The entry whose text is open for selecting in place: a long press selects it, any part of it can be copied.
  const [selecting, setSelecting] = useState<string | null>(null);
  useEffect(() => {
    if (!issue) return;
    const timer = setInterval(() => setNow(Date.now()), 5_000);
    return () => clearInterval(timer);
  }, [issue]);
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  const [pending, setPending] = useState<Pending[]>([]);
  // Opened from search ("said in conversations"), the conversation arrives found.
  const [find, setFind] = useState(route.params.find ?? '');
  const [findOpen, setFinding] = useState(!!route.params.find);
  const [draft, setDraft] = useState(() => draftCache.current.get(target) ?? '');
  const draftTouched = useRef(false);
  useEffect(() => {
    if (draftTouched.current) { if (!draft) draftCache.current.delete(target); return; }
    const restored = draftCache.current.get(target);
    if (restored) setDraft(restored);
  }, [draftsReady, target]);
  useEffect(() => { saveDrafts(); }, [draft, draftsReady]);
  const [away, setAway] = useState(false);
  // Scrolled a little way up from the newest entry: new entries must then not move what is read.
  const [reading, setReading] = useState(false);
  // Dictation: listening, what is heard so far, the microphone's recent levels for a waveform,
  // and whether the draft came from dictation (it is sent tagged so, as it may hold mistakes).
  const [listening, setListening] = useState(false), [heard, setHeard] = useState(''), [levels, setLevels] = useState<number[]>([]);
  const [dictated, setDictated] = useState(false);
  // Images to send with the next message, picked or pasted, and why any were refused.
  const [images, setImages] = useState<Picked[]>([]);
  const [imageIssue, setImageIssue] = useState('');
  // Why the last send did not go, shown above the box while its text is back in it.
  const [sendIssue, setSendIssue] = useState('');
  const limit = messageLimit(caps);
  const sizeInfo = useMemo(() => draftSize(draft, limit), [draft, limit]);
  const stopListening = useRef<(() => Promise<string>) | null>(null);
  const box = useRef<TextInput>(null);
  /** Empty the message box. Clearing it natively too lets it shrink back to one line; with the
   * value alone it kept the height of what was sent (Nathan, 2026-10-02). */
  const emptyBox = () => { draftTouched.current = true; setDraft(''); draftCache.current.delete(target); box.current?.clear(); };
  // Leaving the conversation while listening stops it, so the microphone and other audio are freed.
  useEffect(() => () => { const stop = stopListening.current; stopListening.current = null; void stop?.(); }, []);
  const canDictate = useMemo(() => dictationAvailable(), []);
  // How far from the newest entry the list is scrolled (it is inverted: 0 is the newest).
  const offset = useRef(0);
  const list = useRef<FlatList<Row>>(null);
  const imageViewport = useMemo(() => new ConversationImageViewport(), []);
  const imageViewportBox = useRef<View>(null);
  const measureImageViewport = () => imageViewportBox.current?.measureInWindow((x, y, width, height) => imageViewport.setBounds({ x, y, width, height }));
  useFocusEffect(useCallback(() => {
    imageViewport.setActive(true);
    return () => imageViewport.setActive(false);
  }, [imageViewport]));
  const headerHeight = useHeaderHeight();
  const insets = useSafeAreaInsets();
  // Above the keyboard the composer needs no home-indicator inset.
  const [keyboard, setKeyboard] = useState(false);
  useEffect(() => {
    const shown = Keyboard.addListener('keyboardWillShow', () => setKeyboard(true));
    const hidden = Keyboard.addListener('keyboardWillHide', () => setKeyboard(false));
    return () => { shown.remove(); hidden.remove(); };
  }, []);
  const bottom = keyboard ? 8 : Math.max(insets.bottom, 8);

  useLayoutEffect(() => {
    const runtime = agent && agent.runtime_ids.length && !['stopped', 'failed'].includes(agent.state) ? agent.runtime_ids[0] : undefined;
    navigation.setOptions({
      title,
      // A native bar button: the agent's live terminal, while it has one.
      // Find opens a field above the conversation. A native header search bar blurred an
      // inverted conversation and took its taps.
      unstable_headerRightItems: () => [
        { type: 'button' as const, label: 'Find', icon: { type: 'sfSymbol' as const, name: 'magnifyingglass' as const }, onPress: () => setFinding(open => !open) },
        ...(runtime ? [{
          type: 'button' as const, label: 'Terminal', icon: { type: 'sfSymbol' as const, name: 'terminal' as const }, disabled: status !== 'online',
          onPress: () => void actions.runtimeTerminal(runtime).then(terminalId => { if (terminalId) navigation.navigate('Terminal', { terminalId, title }); }),
        }] : []),
      ],
    });
  }, [navigation, title, agent, status, actions]);

  // st follows the session an agent had when the subscription started. After the agent restarts
  // into a new session, subscribe again, once per new session, or this would keep showing the old
  // one (and a message sent to the new one would never show up here).
  const [followed, setFollowed] = useState<string | undefined>(undefined);
  const currentSession = agent?.current_session_id ?? undefined;
  useEffect(() => {
    if (currentSession && timeline.sessionId && timeline.sessionId !== currentSession && followed !== currentSession) setFollowed(currentSession);
  }, [currentSession, timeline.sessionId, followed]);
  const qualityOpen = useRef(0);
  useFocusEffect(useCallback(() => {
    qualityOpen.current = quality.beginConversation();
    if (unresolved) quality.conversationError(qualityOpen.current, 'transcript-not-bound');
    return () => quality.endConversation(qualityOpen.current);
  }, [target, unresolved, attempt]));
  const visibleEntries = useRef((info: { viewableItems: ViewToken<Row>[] }) => {
    for (const item of info.viewableItems) {
      const kind = item.item.kind;
      if (item.isViewable && (kind === 'entry' || kind === 'call' || kind === 'bundle')) { quality.conversationVisible(qualityOpen.current); break; }
    }
  });
  // The socket sends the newest page, then each change; it never polls. Only while visible.
  useFocusEffect(useCallback(() => {
    if (!feed || unresolved) return;
    setIssue('');
    setStopped(false);
    setRefreshing(true);
    const follow = feed.followConversation(target, {
      onEntries: frame => setTimeline(previous => {
        setLastFrame(Date.now());
        setRefreshing(false);
        setLoaded(true);
        const next = applyConversation(previous, frame);
        rememberBounded(conversationCache.current, target, next, 24);
        return next;
      }),
      onIssue: (text, refused = false) => { setIssue(text); setStopped(refused); },
      onQualityError: code => quality.conversationError(qualityOpen.current, code),
    });
    return () => follow.close();
  }, [feed, target, unresolved, conversationCache, followed, attempt]));

  const names = useMemo(() => {
    const map = new Map(data.agents.map(candidate => [candidate.id, agentName(candidate)]));
    // The phone's session acts for a person (`person/NAME/session/…`); mail names the person.
    const person = sessionPerson(caps?.session_actor);
    if (caps?.session_actor) map.set(caps.session_actor, 'you');
    if (person) map.set(person, 'you');
    return map;
  }, [data.agents, caps?.session_actor]);
  // Half a conversation is worse than none: when st could not read the transcript, say why.
  const unreadable = useMemo(() => unreadableTranscript(timeline.entries), [timeline.entries]);
  useEffect(() => { if (unreadable) quality.conversationError(qualityOpen.current, 'transcript-not-bound'); }, [unreadable]);
  // An entry that did not change keeps its object, so its row is not drawn and measured again:
  // redrawing every row on each frame made the list jump.
  const stable = useRef(new Map<string, { key: string; entry: ConversationEntry }>());
  const entries = useMemo(() => {
    const fresh = unreadable ? [] : conversationEntries(timeline.entries, names);
    const kept = new Map<string, { key: string; entry: ConversationEntry }>();
    const result = fresh.map(entry => {
      const key = JSON.stringify(entry);
      const previous = stable.current.get(entry.id);
      const same = previous && previous.key === key ? previous.entry : entry;
      kept.set(entry.id, { key, entry: same });
      return same;
    });
    stable.current = kept;
    return result;
  }, [unreadable, timeline.entries, names]);
  // A message st has taken shows up in the conversation; the pending copy then gives way.
  useEffect(() => {
    setPending(previous => previous.filter(item => item.failed || !entries.some(entry => entry.body.kind === 'mail' && entry.body.from === 'you' && entry.body.text.trim() === item.text.trim())));
  }, [entries]);
  const finding = findOpen && find.trim() !== '';
  const found = useMemo(() => finding ? entries.filter(entry => entryMatches(entry, find)) : entries, [entries, find, finding]);
  const sourceRevisions = useMemo(() => Object.fromEntries(timeline.entries.map(entry => [entry.id, entry.revision])), [timeline.entries]);
  // Error cards keep their disclosure controls even in this phone's brief mode;
  // grouping them into a one-line tool bundle would hide the original failure.
  const simpleRows = useMemo(() => {
    if (!simpleOn || finding) return [];
    const rows: SimpleRow[] = [];
    let start = 0;
    for (let index = 0; index < found.length; index++) {
      const entry = found[index];
      if (entry.body.kind !== 'tool' || !entry.body.title.startsWith('assistant error')) continue;
      rows.push(...simplify(found.slice(start, index), open), { kind: 'entry', entry });
      start = index + 1;
    }
    rows.push(...simplify(found.slice(start), open));
    return rows;
  }, [found, open, simpleOn, finding]);
  const rows: Row[] = useMemo(() => [
    ...(finding ? [] : [...pending].reverse().map(item => ({ kind: 'pending' as const, pending: item }))),
    // Simplified (this phone's choice, on by default): a tool call to a line, a run to one line.
    ...(simpleOn && !finding
      ? [...simpleRows].reverse().map((row): Row => row)
      : [...found].reverse().map(entry => ({ kind: 'entry' as const, entry }))),
    ...(olderNote(timeline) && !finding ? [{ kind: 'older' as const }] : []),
  ], [found, pending, timeline, finding, simpleOn, open, simpleRows]);

  const canSend = !!agent && status === 'online';
  async function send(spoken?: string) {
    // Words sent straight from dictation (its send button) skip the box.
    const text = (spoken ?? draft).trim();
    const sending = images;
    if (!agent || (!text && !sending.length)) return;
    const item: Pending = { id: `pending-${Date.now()}`, text, at: nowClock(), ...(sending.length ? { images: sending.length } : {}) };
    setPending(previous => [...previous, item]);
    // The words sent from dictation include what was in the box, so it empties either way; a send that
    // does not go puts the text back, and one that st acknowledged leaves it empty.
    emptyBox();
    setImages([]); setImageIssue('');
    // To the newest, where the message appears: animated from nearby, a jump from far up, since an
    // animation across the whole conversation reads as the list scrolling everything again.
    list.current?.scrollToOffset({ offset: 0, animated: offset.current < 1200 });
    const tags = dictated || spoken !== undefined ? ['dictated'] : undefined;
    setDictated(false);
    const sent = await actions.send(agent.id, text, agent.current_session_id ?? undefined, tags, sending);
    setSendIssue('');
    // Images that did not go stay ready to send again.
    if (sent.how === 'failed' && sending.length) setImages(previous => addImages(sending, previous).images);
    if (sent.how === 'failed') {
      // The text goes back in the box, whatever the reason: only a send st acknowledged clears it. Words
      // typed while it was on its way stay after it. The cache holds the box's text even if the person
      // has left the conversation, so the text is there when they come back.
      const restored = restoreDraft(draftCache.current.get(target) ?? '', text);
      if (restored) rememberBounded(draftCache.current, target, restored, 24);
      draftTouched.current = true;
      if (tags) setDictated(true);
      setDraft(restored);
      saveDrafts();
      setPending(previous => previous.filter(candidate => candidate.id !== item.id));
      setSendIssue(`Not sent: ${sendReason(sent.error, false)}`);
      return;
    }
    settle(item.id, sent);
  }
  // An unconfirmed message goes again as the same request, so st keeps at most one of it.
  async function sendAgain(item: Pending) {
    if (!item.unconfirmed || item.resending) return;
    setPending(previous => previous.map(candidate => candidate.id === item.id ? { ...candidate, resending: true } : candidate));
    settle(item.id, await actions.resend(item.unconfirmed));
  }
  // A sent message waits to show up in the conversation; one st has not confirmed keeps its request.
  function settle(id: string, sent: Sent) {
    setPending(previous => previous.map(candidate => candidate.id !== id ? candidate
      : { ...candidate, resending: false, failed: sent.how === 'failed' ? sent.error : undefined, unconfirmed: sent.how === 'unconfirmed' ? sent.request : undefined }));
    if (sent.how === 'sent') setTimeout(() => setPending(previous => previous.filter(candidate => candidate.id !== id)), 60_000);
  }
  // An image from the photo library or the clipboard, for the next message.
  function attach() {
    const pick = async () => {
      const result = await ImagePicker.launchImageLibraryAsync({ mediaTypes: ['images'], allowsMultipleSelection: true, selectionLimit: Math.max(1, MAX_IMAGES - images.length), base64: true, quality: 0.85 });
      if (result.canceled) return;
      const more = result.assets.map(asset => asset.base64 ? picked(asset.base64, asset.mimeType ?? 'image/jpeg', asset.fileName ?? undefined) : `${asset.fileName ?? 'That image'} could not be read.`);
      const next = addImages(images, more);
      setImages(next.images); setImageIssue(next.refused);
    };
    const paste = async () => {
      const image = await Clipboard.getImageAsync({ format: 'png' });
      const next = addImages(images, [image ? fromDataUri(image.data, 'Pasted image') : 'The clipboard holds no image.']);
      setImages(next.images); setImageIssue(next.refused);
    };
    ActionSheetIOS.showActionSheetWithOptions({ options: ['Photo library', 'Paste image', 'Cancel'], cancelButtonIndex: 2 }, index => {
      if (index === 0) void pick().catch(error => setImageIssue(String(error instanceof Error ? error.message : error)));
      if (index === 1) void paste().catch(error => setImageIssue(String(error instanceof Error ? error.message : error)));
    });
  }
  async function listen() {
    setHeard(''); setLevels([]);
    try {
      stopListening.current = await startDictation({
        onText: setHeard,
        onLevel: level => setLevels(previous => [...previous.slice(-39), level]),
        onError: message => setIssue(`Dictation stopped: ${message}`),
      });
      setListening(true);
    } catch (error) { setIssue(`Dictation could not start: ${error instanceof Error ? error.message : String(error)}`); }
  }
  async function finishListening(then: 'cancel' | 'edit' | 'send') {
    const stop = stopListening.current;
    stopListening.current = null;
    setListening(false);
    const words = stop ? (await stop()).trim() : '';
    if (then === 'cancel' || !words) return;
    if (then === 'send') { await send(draft.trim() ? `${draft.trimEnd()} ${words}` : words); return; }
    const text = draft.trim() ? `${draft.trimEnd()} ${words}` : words;
    setDraft(text); rememberBounded(draftCache.current, target, text, 24);
    setDictated(true);
  }
  // Debug builds only: type a message and send it, as a person would, so a recording can show
  // what the list does on send (EXPO_PUBLIC_ST3_TEST_SEND).
  const [devSend, setDevSend] = useState(0);
  useEffect(() => {
    const text = __DEV__ ? process.env.EXPO_PUBLIC_ST3_TEST_SEND : undefined;
    if (!text || !loaded || !agent) return;
    // `\n` in the variable is a new line, so a recording can show a tall box after a send.
    const typed = setTimeout(() => setDraft(`${text.replaceAll('\\n', '\n')} ${new Date().toISOString().slice(11, 19)}`), 3_000);
    const sent = setTimeout(() => setDevSend(count => count + 1), 4_500);
    return () => { clearTimeout(typed); clearTimeout(sent); };
  }, [loaded, agent?.id]);
  useEffect(() => { if (devSend) void send(); }, [devSend]);
  // Debug builds only: scroll back to the oldest entry every few seconds, as a person reading
  // back would, so a recording can show earlier pages arriving (EXPO_PUBLIC_ST3_TEST_SCROLL_BACK
  // is how many times).
  useEffect(() => {
    let left = __DEV__ ? Number(process.env.EXPO_PUBLIC_ST3_TEST_SCROLL_BACK ?? 0) : 0;
    if (!left || !loaded) return;
    const timer = setInterval(() => {
      if (left-- <= 0) { clearInterval(timer); return; }
      dragged.current = true;
      list.current?.scrollToEnd({ animated: false });
    }, 4_000);
    return () => clearInterval(timer);
  }, [loaded]);
  const toggle = useCallback((id: string) => setOpen(previous => { const next = new Set(previous); if (next.has(id)) next.delete(id); else next.add(id); return next; }), []);
  // A subagent card opens its child conversation as its own, through the session's own route.
  const openSession = useCallback((sessionId: string, title: string) => openSubagentConversation(navigation, sessionId, title), [navigation]);
  // The conversation header, as st last sent it: what is running, what it costs, what waits.
  const header = useMemo(() => headerLine(timeline.header, new Date(lastFrame ?? Date.now()).toISOString()), [timeline.header, lastFrame]);

  return <KeyboardAvoidingView style={styles.screen} behavior={Platform.OS === 'ios' ? 'padding' : undefined} keyboardVerticalOffset={headerHeight}>
    <Banners />
    {agent ? <AgentStrip agent={agent} onMission={(id, missionTitle) => navigation.navigate('Mission', { id, title: missionTitle })} /> : session ? <View style={styles.strip}><T dim numberOfLines={1}>{session.driver ?? 'harness'} · {session.state} · {session.id}</T></View> : null}
    {header ? <View style={styles.strip}><T dim>{header}</T></View> : null}
    {agent ? <AlertsStrip agentId={agent.id} entries={timeline.entries} /> : null}
    {issue ? <Pressable accessibilityRole={stopped ? 'button' : undefined} disabled={!stopped} onPress={retry} style={styles.strip}><T color={theme.waiting}>{staleLine(issue, loaded, lastFrame, now, stopped)}</T></Pressable> : null}
    {!unresolved && !issue ? <Freshness refreshing={refreshing} loaded={loaded} lastFrame={lastFrame} /> : null}
    {findOpen ? <View style={[styles.strip, { flexDirection: 'row', alignItems: 'center', gap: 8 }]}>
      <Field value={find} onChangeText={setFind} placeholder="Find in this conversation" autoFocus autoCapitalize="none" autoCorrect={false} spellCheck={false} returnKeyType="search" style={{ flex: 1 }} />
      <Pressable accessibilityRole="button" onPress={() => { setFinding(false); setFind(''); }}><T color={theme.accent}>Done</T></Pressable>
    </View> : null}
    {finding ? <View style={styles.strip}><T color={theme.yellow}>{found.length === 0 ? `Nothing here says “${find.trim()}”` : `${found.length} ${found.length === 1 ? 'entry says' : 'entries say'} “${find.trim()}”`}</T></View> : null}
    <View ref={imageViewportBox} collapsable={false} style={{ flex: 1 }} onLayout={measureImageViewport}>
    <FlatList
      ref={list}
      onViewableItemsChanged={visibleEntries.current}
      inverted
      data={rows}
      extraData={selecting}
      keyExtractor={row => row.kind === 'entry' ? row.entry.id : row.kind === 'call' ? `call:${row.entry.id}` : row.kind === 'bundle' ? row.id : row.kind === 'pending' ? row.pending.id : 'older'}
      renderItem={({ item: row }) => row.kind === 'older'
        ? <Pressable accessibilityRole="button" onPress={() => { dragged.current = true; loadOlder(); }} style={styles.entry}><T dim>{olderNote(timeline)}</T></Pressable>
        : row.kind === 'pending' ? <PendingView pending={row.pending} canSend={canSend} onSendAgain={sendAgain} />
        : row.kind === 'bundle' ? <BundleView row={row} onToggle={toggle} />
        : row.kind === 'call' && !open.has(row.entry.id) ? <CallView entry={row.entry} tool={row.tool} onToggle={toggle} />
        // A long press turns the entry into selectable text where it is, with the handles and menu of
        // iOS; iOS paragraphs select only whole, a text view selects any part. Tapping away puts it back.
        : row.entry.id === selecting
          ? <TextInput accessibilityLabel="Entry text, selected for copying" style={styles.selecting} value={entryText(row.entry)} multiline autoFocus selectTextOnFocus showSoftInputOnFocus={false} scrollEnabled={false} autoCorrect={false} spellCheck={false} onChangeText={() => undefined} onBlur={() => setSelecting(null)} />
          : <Pressable accessible={false} onLongPress={() => setSelecting(row.entry.id)} delayLongPress={350}><EntryView entry={row.entry} conversationId={timeline.sessionId ?? sessionId ?? target} revisionKey={row.entry.contentSources?.map(source => `${source.id}:${sourceRevisions[source.id] ?? 0}`).join('|') ?? String(sourceRevisions[row.entry.id] ?? 0)} imageViewport={imageViewport} open={open.has(row.entry.id)} onToggle={toggle} brief={simpleOn && !finding} onOpenSession={openSession} /></Pressable>}
      ListEmptyComponent={<View style={[styles.entry, { transform: [{ scaleY: -1 }] }]}>{unreadable ? <T color={theme.waiting} selectable>{unreadable}</T> : <Pressable accessibilityRole={stopped ? 'button' : undefined} disabled={!stopped} onPress={retry}><T dim>{unresolved ? 'This process has no exact native session history.' : !loaded ? (issue ? staleLine(issue, false, null, now, stopped) : status === 'online' ? 'Loading the conversation…' : 'Offline; this conversation has not been loaded yet.') : 'No conversation in the recent timeline.'}</T></Pressable>}</View>}
      // Scrolled up, a new entry must not move what is being read, so the position is kept. At
      // the newest it must not be: the position-keeping scrolls to a new entry with an animation,
      // which slid the whole conversation after each send and each reply (Nathan, 2026-10-05).
      // An inverted list anchored at its newest end simply grows there, without moving.
      maintainVisibleContentPosition={reading ? { minIndexForVisible: 0 } : undefined}
      keyboardDismissMode="interactive"
      // A tap on the conversation puts the keyboard away, as in Messages; the next tap acts.
      keyboardShouldPersistTaps="never"
      onScroll={event => { offset.current = event.nativeEvent.contentOffset.y; setAway(offset.current > 240); setReading(offset.current > 60); imageViewport.refresh(); }}
      onContentSizeChange={measureImageViewport}
      onScrollBeginDrag={() => { dragged.current = true; }}
      // Inverted: the end is the oldest entry. Reaching it reads the page before.
      onEndReached={loadOlder}
      onEndReachedThreshold={0.5}
      scrollEventThrottle={100}
      contentContainerStyle={{ paddingVertical: 8 }}
    />
    </View>
    {away ? <Pressable accessibilityRole="button" accessibilityLabel="Scroll to latest" style={styles.latest} onPress={() => list.current?.scrollToOffset({ offset: 0, animated: true })}><T bold color={theme.accent}>↓ latest</T></Pressable> : null}
    {agent && listening ? <View style={[styles.composer, { paddingBottom: bottom, flexDirection: 'column', alignItems: 'stretch' }]}>
      <Waveform levels={levels} />
      <T color={heard ? theme.text : theme.overlay0}>{heard || 'Listening…'}</T>
      <View style={{ flexDirection: 'row', gap: 8, justifyContent: 'flex-end' }}>
        <Button label="cancel" color={theme.overlay1} onPress={() => void finishListening('cancel')} />
        <Button label="done" color={theme.subtext0} onPress={() => void finishListening('edit')} />
        <Button label="send" disabled={!canSend} onPress={() => void finishListening('send')} />
      </View>
    </View> : agent ? <View>
    {images.length || imageIssue ? <View style={styles.tray}>
      {images.map(image => <View key={image.key}>
        <Image source={{ uri: `data:${image.mediaType};base64,${image.base64}` }} style={styles.thumb} accessibilityLabel={image.name ?? 'image to send'} />
        <Pressable accessibilityRole="button" accessibilityLabel={`Remove ${image.name ?? 'image'}`} hitSlop={8} onPress={() => setImages(previous => previous.filter(other => other.key !== image.key))} style={styles.unthumb}><T bold color={theme.text}>✕</T></Pressable>
        <T dim style={{ fontSize: 10 }}>{megabytes(image.bytes)}</T>
      </View>)}
      {imageIssue ? <T color={theme.waiting} style={{ flex: 1 }}>{imageIssue}</T> : null}
    </View> : null}
    {sendIssue || sizeInfo.note ? <View style={styles.tray} accessibilityLiveRegion="polite"><T color={sendIssue || sizeInfo.over ? theme.waiting : theme.overlay1} style={{ flex: 1 }}>{sendIssue || sizeInfo.note}</T></View> : null}
    <View style={[styles.composer, { paddingBottom: bottom }]}>
      <Pressable accessibilityRole="button" accessibilityLabel="Attach an image" hitSlop={8} onPress={attach} disabled={images.length >= MAX_IMAGES} style={styles.prompt}>
        <T color={images.length >= MAX_IMAGES ? theme.overlay0 : theme.accent} style={{ fontSize: 18 }}>＋</T>
      </Pressable>
      {/* The microphone sits inside the box, at its right edge, over the text's padding. */}
      <View style={{ flex: 1 }}>
        <Field
          ref={box}
          value={draft}
          onChangeText={text => { draftTouched.current = true; setSendIssue(''); setDraft(text); if (text) rememberBounded(draftCache.current, target, text, 24); else draftCache.current.delete(target); }}
          placeholder={`Message ${title}`}
          multiline
          style={[styles.input, canDictate ? { paddingRight: 36 } : null]}
          editable
          accessibilityLabel={`Message ${title}`}
        />
        {canDictate ? <Pressable accessibilityRole="button" accessibilityLabel="Dictate a message" hitSlop={8} onPress={() => void listen()} style={styles.mic}>
          <T style={{ fontSize: 17 }}>🎙</T>
        </Pressable> : null}
      </View>
      <Button label="send" disabled={!canSend || sizeInfo.over || (!draft.trim() && !images.length)} onPress={() => void send()} style={styles.send} />
    </View>
    </View> : session ? <View style={[styles.composer, { paddingBottom: bottom }]}><T dim>{session.managed === false ? 'not started by st · read only' : 'this session has ended · read only'}</T></View> : null}
  </KeyboardAvoidingView>;
}

function AgentStrip({ agent, onMission }: { agent: NonNullable<ReturnType<typeof useStore>['data']['agents'][number]>; onMission: (id: string, title: string) => void }) {
  const state = agentState(agent);
  const { glyph, color } = agentGlyph(state);
  const harness = harnessName(agent.driver);
  const work = agent.current_work?.[0];
  return <View style={styles.strip}>
    <View style={{ flexDirection: 'row', alignItems: 'center' }}>
      <T bold color={color}>{glyph} </T><T bold>{agentName(agent)}</T><T color={color}>  {agentReported(agent) ?? agentWord(state)}</T>
      <View style={{ flex: 1 }} />
      <T color={harnessColor(harness)}>{harness}</T><T dim>{agentModel(agent) ? ` · ${agentModel(agent)}` : ''} · {agent.host_id?.replace(/^host\//, '') ?? '?'}</T>
    </View>
    <T dim numberOfLines={1}>{agent.id}</T>
    {agent.checkout ? <T soft>{checkoutLabel(agent.checkout)}</T> : null}
    {state === 'needs-login' ? <T color={theme.person}>⚿ {loginGuidance(agent)}</T> : null}
    {work ? <Pressable onPress={() => onMission(work.mission_id, work.title || work.path)}><T numberOfLines={1}><T dim>mission </T><T color={theme.accent}>{work.mission_id.replace(/^mission\//, '')} › {work.path}</T></T></Pressable> : null}
  </View>;
}

const PendingView = memo(function PendingView({ pending, canSend, onSendAgain }: { pending: Pending; canSend: boolean; onSendAgain: (pending: Pending) => void }) {
  const rule = RULES.pending;
  const tone = pending.failed ? rule.failed : pending.unconfirmed ? rule.unconfirmed : null;
  const label = pending.failed ? `you · not sent: ${pending.failed}`
    : pending.unconfirmed ? (pending.resending ? 'you · sending again…' : 'you · st may have taken it and has not confirmed yet')
    : rule.sending.text;
  return <View style={[styles.entry, styles.barred, { borderLeftColor: c(tone ?? rule.sending_edge) }]}>
    <T color={c(tone ?? rule.sending.color)}>{label}<T dim>  {pending.at}{pending.images ? `  🖼 ${pending.images}` : ''}</T></T>
    <Markdown selectable={false} text={pending.text} color={c(rule.text)} />
    {pending.unconfirmed ? <Button label="Send again" color={c(rule.unconfirmed)} disabled={!canSend || pending.resending} onPress={() => onSendAgain(pending)} /> : null}
  </View>;
});

/** How fresh the conversation is, as stui says it on the rule above its box: live, how long since st
 * last sent it, and that the phone is asking again. It keeps its own clock so a tick redraws only this line. */
function Freshness({ refreshing, loaded, lastFrame }: { refreshing: boolean; loaded: boolean; lastFrame: number | null }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 5_000);
    return () => clearInterval(timer);
  }, []);
  const line = freshnessLine({ issue: '', loaded, refreshing, lastFrame, now });
  return <View style={styles.freshness} accessible accessibilityLabel={line.text}><T dim={line.tone === 'live'} color={line.tone === 'refreshing' ? theme.accent : undefined} numberOfLines={1}>{line.text}</T></View>;
}

const EntryView = memo(function EntryView(props: { entry: ConversationEntry; conversationId: string; revisionKey: string; imageViewport: ConversationImageViewport; open: boolean; onToggle: (id: string) => void; brief?: boolean; onOpenSession?: (sessionId: string, title: string) => void }) {
  return <View>
    <EntryBodyView {...props} />
    {props.entry.content?.map(reference => reference.reason === 'on-demand' || reference.media_type.startsWith('image/')
      ? <ConversationImageView key={`${props.conversationId}:${props.entry.id}:${props.revisionKey}:${reference.ref}`} conversationId={props.conversationId} reference={reference} viewport={props.imageViewport} />
      : <FetchedContent key={`${props.conversationId}:${reference.ref}`} conversationId={props.conversationId} reference={reference} entry={props.entry} onOpenSession={props.onOpenSession} />)}
  </View>;
});

type NativeImage = { uri: string; size: number };
type ConversationImageProps = { conversationId: string; reference: ConversationContentRef; viewport: ConversationImageViewport; client: St3Client | null };

// Class lifecycle owns native measurement and invalidates late responses on unmount.
// All mounted images share the viewport queue; FlatList overscan never authorizes reads.
class VisibleConversationImageView extends Component<ConversationImageProps, VisibleImageState<NativeImage>> {
  state: VisibleImageState<NativeImage> = { kind: 'closed' };
  private box = createRef<View>();
  private unregister?: () => void;
  private image = new VisibleConversationImage<NativeImage>(this.props.viewport, async current => {
    const { client, conversationId, reference } = this.props;
    if (!client) throw new Error('Connect to the server and try again.');
    const content = await loadConversationContent(reference, async (ref, offset) => {
      if (!current()) throw new Error('Image is no longer visible.');
      return (await client.conversationContentChunk(conversationId, ref, offset)).value;
    });
    return { uri: contentImageUri(content), size: content.bytes.length };
  }, state => this.setState(state));

  componentDidMount() {
    this.unregister = this.props.viewport.register({
      measure: done => this.box.current?.measureInWindow((x, y, width, height) => done({ x, y, width, height })),
      visible: shown => this.image.setVisible(shown),
    });
  }

  componentWillUnmount() { this.image.dispose(); this.unregister?.(); }

  render() {
    const state = this.state;
    return <View style={styles.entry}>
      <View ref={this.box} collapsable={false} style={styles.mailImage} onLayout={() => this.props.viewport.refresh()}>
        {state.kind === 'image'
          ? <Image accessibilityLabel="Fetched conversation image" source={{ uri: state.image.uri, cache: 'reload' }} style={StyleSheet.absoluteFill} resizeMode="contain" onError={() => this.image.fail('The image could not be displayed. Try again.')} />
          : <T dim style={{ padding: 6 }}>{state.kind === 'failed' ? 'Image unavailable' : state.kind === 'loading' ? 'Loading image…' : 'Image'}</T>}
      </View>
      {state.kind === 'image' ? <T dim>{megabytes(state.image.size)}</T> : null}
      {state.kind === 'failed' ? <View>
        <T color={theme.red}>Could not load image: {state.message}</T>
        <Pressable accessibilityRole="button" onPress={() => this.image.retry()}><T color={theme.accent}>Retry loading image</T></Pressable>
      </View> : null}
    </View>;
  }
}

function ConversationImageView(props: Omit<ConversationImageProps, 'client'>) {
  const { client } = useStore();
  return <VisibleConversationImageView {...props} client={client} />;
}

// Explicit disclosure only. Identifiable bodies/payloads use the normal typed
// output view; ambiguous metadata/view references retain a lossless JSON fallback.
function FetchedContent({ conversationId, reference, entry, onOpenSession }: { conversationId: string; reference: ConversationContentRef; entry: ConversationEntry; onOpenSession?: (sessionId: string, title: string) => void }) {
  const { client } = useStore();
  const [state, setState] = useState<{ kind: 'closed' } | { kind: 'loading' } | { kind: 'failed'; message: string } | { kind: 'json'; text: string } | { kind: 'typed'; entries: ConversationEntry[] }>({ kind: 'closed' });
  const load = async () => {
    setState({ kind: 'loading' });
    try {
      if (!client) throw new Error('Connect to the server and try again.');
      const content = await loadConversationContent(reference, async (ref, offset) => (await client.conversationContentChunk(conversationId, ref, offset)).value);
      const value = contentJsonValue(content);
      const entries = fetchedConversationEntries(entry, reference, value);
      setState(entries?.length ? { kind: 'typed', entries } : { kind: 'json', text: JSON.stringify(value, null, 2) });
    } catch (error) { setState({ kind: 'failed', message: error instanceof Error ? error.message : String(error) }); }
  };
  const expanded = state.kind === 'json' || state.kind === 'typed';
  return <View style={styles.entry}>
    <Pressable accessibilityRole="button" accessibilityState={{ expanded, disabled: state.kind === 'loading' }} disabled={state.kind === 'loading'} onPress={() => expanded ? setState({ kind: 'closed' }) : void load()}>
      <T color={theme.accent}>{state.kind === 'loading' ? 'Loading…' : expanded ? 'Show less' : state.kind === 'failed' ? 'Retry loading' : 'Show all'}<T dim>{reference.size === undefined ? '' : ` · ${megabytes(reference.size)}`}</T></T>
    </Pressable>
    {state.kind === 'failed' ? <T color={theme.red}>Could not load full content: {state.message}</T> : null}
    {state.kind === 'json' ? <T selectable>{state.text}</T> : null}
    {state.kind === 'typed' ? state.entries.map(shown => <EntryBodyView key={shown.id} entry={shown} open onToggle={() => setState({ kind: 'closed' })} onOpenSession={onOpenSession} />) : null}
  </View>;
}

const EntryBodyView = memo(function EntryBodyView({ entry, open, onToggle, brief = false, onOpenSession }: { entry: ConversationEntry; open: boolean; onToggle: (id: string) => void; brief?: boolean; onOpenSession?: (sessionId: string, title: string) => void }) {
  const body = entry.body;
  switch (body.kind) {
    case 'user':
      return <View style={[styles.user, { backgroundColor: c(RULES.user.fill) }]}>
        <T color={c(RULES.user.time)} style={{ alignSelf: 'flex-end' }}>{entry.at}</T>
        <T color={c(RULES.user.text)}>{body.text}</T>
      </View>;
    case 'assistant':
      return <View style={styles.entry}><Markdown selectable={false} text={body.text} color={c(RULES.assistant)} /></View>;
    case 'mail': return <MailView entry={entry} body={body} open={open} onToggle={onToggle} brief={brief} />;
    case 'event': {
      const color = body.tone === 'fault' ? theme.red : body.tone === 'warning' ? theme.yellow : c(RULES.event.label);
      return <View style={[styles.entry, { flexDirection: 'row' }]}>
        <T color={c(RULES.event.rule)}>── </T><T color={color} style={{ flex: 1 }}>{body.text}{entry.at ? ` · ${entry.at}` : ''}</T>
      </View>;
    }
    case 'tool': {
      // Quiet until opened, as in stui: an edge in the outcome's colour, dim text, no fill, and
      // at most six one-line rows; opened, it reads at full brightness.
      const rule = RULES.tool;
      const [glyph, color] = body.state === 'running' ? ['⠿', c(rule.running)] : body.state === 'ok' ? [rule.ok.text, c(rule.ok.color)] : [rule.failed.text, c(rule.failed.color)];
      const shown = shownToolLines(body, open);
      // Even a short assignment/code block can contain wrapped lines hidden by quiet mode.
      const expandable = body.output.length > 0;
      const quiet = !open;
      const look = quiet ? rule.quiet : rule.open;
      // A subagent card's `open session/…` line is the link to that conversation (q2).
      const session = subagentSession(body.output);
      return <View accessible={false} style={[styles.tool, { borderLeftColor: color }]}>
        <Pressable accessible={expandable} accessibilityRole={expandable ? 'button' : undefined} accessibilityLabel={expandable ? `${body.title}, ${open ? 'collapse' : 'expand'} output` : undefined} accessibilityState={expandable ? { expanded: open } : undefined} disabled={!expandable} onPress={() => onToggle(entry.id)}>
          <T numberOfLines={1}><T bold color={color}>{glyph} </T><T bold={look.title_bold} color={c(look.title)}>{body.title}</T></T>
          {shown.hidden ? <T color={c(rule.collapse.color)}>  … {shown.hidden} more lines · tap to show</T> : null}
          {open && expandable ? <T color={c(rule.collapse.color)}>  {rule.collapse.text}</T> : null}
        </Pressable>
        {shown.lines.filter(line => !session || line !== `open ${session}`).map((line, index) =>
          <T key={index} numberOfLines={quiet ? 1 : undefined} style={[styles.toolLine, quiet && { opacity: 0.7 }]} color={c(line.startsWith('+') ? rule.added : line.startsWith('-') || line.includes('error') ? rule.removed : look.rows)}>{line || ' '}</T>)}
        {session ? <Pressable accessibilityRole="link" accessibilityLabel={`Open subagent conversation ${body.title.split(' · ')[0]}`} onPress={() => onOpenSession?.(session, body.title.split(' · ')[0] ?? session)}><T color={theme.accent} style={styles.toolLine}>↗ open {body.title.split(' · ')[0]}</T></Pressable> : null}
      </View>;
    }
  }
});

/** The microphone's last few levels as bars, newest on the right. */
function Waveform({ levels }: { levels: number[] }) {
  const bars = [...Array(Math.max(0, 40 - levels.length)).fill(0), ...levels];
  return <View style={{ flexDirection: 'row', alignItems: 'center', height: 32, gap: 2 }}>
    {bars.map((level, index) => <View key={index} style={{ flex: 1, height: 3 + level * 28, borderRadius: 2, backgroundColor: theme.accent, opacity: 0.4 + level * 0.6 }} />)}
  </View>;
}

type Tool = Extract<ConversationEntry['body'], { kind: 'tool' }>;

const toolGlyph = (state: Tool['state']): [string, string] => state === 'running' ? ['⠿', c(RULES.tool.running)] : state === 'ok' ? [RULES.tool.ok.text, c(RULES.tool.ok.color)] : [RULES.tool.failed.text, c(RULES.tool.failed.color)];

// Simplified: one tool call on one line, never wrapped; a tap opens it in full.
const CallView = memo(function CallView({ entry, tool, onToggle }: { entry: ConversationEntry; tool: Tool; onToggle: (id: string) => void }) {
  const [glyph, color] = toolGlyph(tool.state);
  return <Pressable accessibilityRole="button" accessibilityState={{ expanded: false }} onPress={() => onToggle(entry.id)} style={[styles.call, { borderLeftColor: color }]}>
    <T numberOfLines={1}><T bold color={color}>{glyph} </T><T color={c(RULES.tool.quiet.title)}>{tool.title}</T></T>
  </Pressable>;
});

// Simplified: a run of calls on one line, "▸ 7 tool calls · ✓6 · ✕1 · last: …"; a tap lists them.
const BundleView = memo(function BundleView({ row, onToggle }: { row: Extract<SimpleRow, { kind: 'bundle' }>; onToggle: (id: string) => void }) {
  const rule = RULES.bundle;
  const edge = row.running ? c(RULES.tool.running) : row.failed ? c(RULES.tool.failed.color) : c(RULES.tool.ok.color);
  return <Pressable accessibilityRole="button" accessibilityState={{ expanded: row.open }} onPress={() => onToggle(row.id)} style={[styles.call, { borderLeftColor: edge }]}>
    <T numberOfLines={1}>
      <T color={c(rule.label)}>{row.open ? rule.opened : rule.folded} {row.calls.length} tool calls</T>
      {row.running ? <T color={c(RULES.tool.running)}> · ⠿{row.running}</T> : null}
      {row.ok ? <T color={c(RULES.tool.ok.color)}> · {RULES.tool.ok.text}{row.ok}</T> : null}
      {row.failed ? <T color={c(RULES.tool.failed.color)}> · {RULES.tool.failed.text}{row.failed}</T> : null}
      <T color={c(rule.last)}> · last: {row.last}</T>
    </T>
  </Pressable>;
});

type Mail = Extract<ConversationEntry['body'], { kind: 'mail' }>;

// Mail the person is part of leads; mail between others stays back and folds to a few rows,
// like a tool call, until tapped. Mail to the person is filled like their own messages, and
// their own says how far it got: ✓ st has it, ✓✓ the agent has it.
const MailView = memo(function MailView({ entry, body, open, onToggle, brief = false }: { entry: ConversationEntry; body: Mail; open: boolean; onToggle: (id: string) => void; brief?: boolean }) {
  const rule = RULES.mail;
  const toYou = body.to === 'you';
  const look = folds(body) ? rule.between_others : rule.involving_you;
  const [height, setHeight] = useState(0);
  const limit = RULES.tool.collapsed_rows * LINE;
  const whole = useWholeText(body.whole);
  const long = folds(body) && height > limit;
  const mark = body.from !== 'you' ? null : body.delivered ? rule.delivered : rule.sent;
  // Simplified, mail between others is two lines until tapped: who to whom and the subject, then
  // its first line (Nathan, 2026-10-02).
  if (brief && folds(body) && !open) {
    const lines = body.text.split('\n').map(line => line.trim()).filter(Boolean);
    return <Pressable accessibilityRole="button" accessibilityState={{ expanded: false }} onPress={() => onToggle(entry.id)}
      style={[styles.entry, styles.barred, { borderLeftColor: c(look.edge) }]}>
      <T numberOfLines={1}><T bold={look.from_bold} color={c(look.from)}>{body.to ? `${body.from} → ${body.to}` : body.from}</T>{body.subject ? <T color={c(look.text)}>  {body.subject}</T> : null}<T dim>  {entry.at}</T>{body.dictated ? <T dim>  🎙</T> : null}{body.signed ? <T dim>  {body.signed}</T> : null}{body.images ? <T dim>  🖼 {body.images.length}</T> : null}</T>
      <T numberOfLines={1} color={c(look.text)}>{lines[0] ?? ''}{lines.length > 1 ? ' …' : ''}</T>
    </Pressable>;
  }
  return <Pressable disabled={!long} accessibilityRole={long ? 'button' : undefined} accessibilityState={long ? { expanded: open } : undefined} onPress={() => onToggle(entry.id)}
    style={[styles.entry, styles.barred, { borderLeftColor: c(look.edge) }, toYou ? { backgroundColor: c(rule.to_you_fill) } : null]}>
    <T>{toYou ? <T color={c(look.edge)}>● </T> : null}<T bold={look.from_bold} color={c(look.from)}>{body.to ? `${body.from} → ${body.to}` : body.from}</T>{body.subject ? <T bold={look.from_bold} color={look.from_bold ? undefined : c(look.text)}>  {body.subject}</T> : null}<T dim>  {entry.at}</T>{body.dictated ? <T dim>  🎙</T> : null}{body.signed ? <T dim>  {body.signed}</T> : null}{mark ? <T color={c(mark.color)}>  {mark.text}</T> : null}</T>
    <View style={long && !open ? { maxHeight: limit, overflow: 'hidden' } : null}>
      {/* Measured at its full height and never shrunk by the fold: a measurement the fold could
          change would fold and unfold the mail in a loop. */}
      <View style={{ flexShrink: 0 }} onLayout={event => { const next = event.nativeEvent.layout.height; setHeight(previous => Math.max(previous, next)); }}>{whole.text ?? body.text ? <Markdown selectable={false} text={whole.text ?? body.text} color={c(look.text)} /> : null}</View>
    </View>
    {body.whole && !(whole.text && !whole.note) ? <Pressable accessibilityRole="button" accessibilityLabel="Show the whole message" disabled={whole.loading} onPress={whole.load} hitSlop={6}>
      <T color={c(RULES.tool.collapse.color)}>{whole.loading ? '  loading the whole message…' : `  show the whole message · ${kib(body.whole.bytes)}`}</T>
    </Pressable> : null}
    {whole.note ? <T color={theme.waiting} style={{ fontSize: 12 }}>{whole.note}</T> : null}
    {body.images ? <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 6, marginTop: 4 }}>{body.images.map(image => <MailImageView key={image.sha256} image={image} />)}</View> : null}
    {long ? <T color={c(RULES.tool.collapse.color)}>{open ? `  ${RULES.tool.collapse.text}` : '  … more · tap to show'}</T> : null}
  </Pressable>;
});

function kib(bytes: number) { return bytes < 1024 ? `${bytes} bytes` : `${Math.round(bytes / 102.4) / 10} KiB`; }

/** The whole text of a long message, read from st on request: the text, a note on why it is partial or missing, and a way to ask. */
function useWholeText(whole: { message: string; bytes: number } | undefined) {
  const { actions } = useStore();
  const [state, setState] = useState<{ text?: string; note?: string; loading: boolean }>({ loading: false });
  const load = () => {
    if (!whole) return;
    setState(previous => ({ ...previous, loading: true }));
    actions.messageBody(whole.message).then(
      answer => setState({ ...wholeText(answer), loading: false }),
      error => setState(previous => ({ text: previous.text, note: wholeTextFailure(error), loading: false })),
    );
  };
  return { ...state, load };
}

// An image a message carries, read from st when shown. Its box keeps one size while it loads,
// so the conversation does not move under the reader.
function MailImageView({ image }: { image: MailImage }) {
  const { actions } = useStore();
  const [uri, setUri] = useState<string | null>(null), [failed, setFailed] = useState('');
  useEffect(() => {
    let live = true;
    actions.image(image).then(next => { if (live) setUri(next); }, error => { if (live) setFailed(error instanceof Error ? error.message : String(error)); });
    return () => { live = false; };
  }, [image.sha256]); // eslint-disable-line react-hooks/exhaustive-deps
  const label = `${image.name ?? 'image'} · ${megabytes(image.size)}`;
  return <View style={styles.mailImage} accessibilityLabel={label}>
    {uri ? <Image source={{ uri }} style={StyleSheet.absoluteFill} resizeMode="contain" />
      : <T dim style={{ fontSize: 11, padding: 6 }}>{failed ? `🖼 ${label}\n${failed}` : `🖼 ${label}…`}</T>}
  </View>;
}

const styles = StyleSheet.create({
  tray: { flexDirection: 'row', alignItems: 'center', gap: 8, paddingHorizontal: 10, paddingTop: 8, backgroundColor: theme.mantle, borderTopColor: theme.surface0, borderTopWidth: StyleSheet.hairlineWidth * 2 },
  thumb: { width: 56, height: 56, borderRadius: 6, backgroundColor: theme.surface0 },
  unthumb: { position: 'absolute', top: -6, right: -6, width: 20, height: 20, borderRadius: 10, backgroundColor: theme.surface1, alignItems: 'center', justifyContent: 'center' },
  mailImage: { width: 200, height: 150, borderRadius: 6, overflow: 'hidden', backgroundColor: theme.surface0 },
  screen: { flex: 1, backgroundColor: theme.base },
  freshness: { paddingHorizontal: 12, paddingVertical: 3, backgroundColor: theme.base },
  strip: { paddingHorizontal: 12, paddingVertical: 6, borderBottomColor: theme.surface0, borderBottomWidth: StyleSheet.hairlineWidth * 2, backgroundColor: theme.base },
  entry: { paddingHorizontal: 12, paddingVertical: 6 },
  selecting: { paddingHorizontal: 12, paddingVertical: 6, color: theme.text, backgroundColor: theme.surface0, fontFamily: fonts.regular, fontSize: 14, lineHeight: LINE },
  user: { backgroundColor: theme.userBg, paddingHorizontal: 12, paddingVertical: 8, marginVertical: 6 },
  barred: { borderLeftWidth: 2, marginLeft: 10, paddingLeft: 8 },
  tool: { marginHorizontal: 8, marginVertical: 4, paddingHorizontal: 8, paddingVertical: 4, borderLeftWidth: 2 },
  toolLine: { fontSize: 12, lineHeight: 17, paddingLeft: 18 },
  call: { marginHorizontal: 8, marginVertical: 2, paddingHorizontal: 8, paddingVertical: 3, borderLeftWidth: 2 },
  composer: { flexDirection: 'row', alignItems: 'flex-end', gap: 6, paddingHorizontal: 10, paddingTop: 6, borderTopColor: theme.surface0, borderTopWidth: StyleSheet.hairlineWidth * 2, backgroundColor: theme.mantle },
  prompt: { paddingBottom: 9, fontFamily: fonts.bold },
  // The box grows with its text up to maxHeight. No flex here: inside the column that holds the
  // microphone, flex would set the height from the column (36) instead of the text.
  input: { minHeight: 36, maxHeight: 140, marginTop: 0 },
  mic: { position: 'absolute', right: 6, bottom: 6, zIndex: 1, padding: 2 },
  send: { marginTop: 0, marginBottom: 2 },
  latest: { position: 'absolute', right: 12, bottom: 84, backgroundColor: theme.surface0, paddingHorizontal: 10, paddingVertical: 6, borderRadius: 4 },
});

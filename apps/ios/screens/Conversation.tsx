import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { FlatList, Keyboard, KeyboardAvoidingView, Platform, Pressable, StyleSheet, View } from 'react-native';
import { useFocusEffect } from '@react-navigation/native';
import { useHeaderHeight } from '@react-navigation/elements';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import type { TimelineEntry } from '../../../clients/typescript/st3-client';
import { agentGlyph, agentName, agentState, agentWord, harnessColor, harnessName } from '../agentsView';
import { Banners } from '../chrome';
import rules from '../../../fixtures/clients/conversation-style.json';
import { tokenColor, type ConversationRules } from '../conversationStyle';
import { COLLAPSED_TOOL_LINES, conversationEntries, entryMatches, entryText, folds, shownToolLines, unreadableTranscript, type ConversationEntry } from '../conversationView';
import { rememberBounded } from '../boundedCache';
import { sessionPerson } from '../homeView';
import type { RootScreen } from '../navigation';
import { applyConversation, isUnresolved, type Conversation } from '../sessionView';
import { useStore } from '../store';
import { fonts, theme } from '../theme';
import { Button, Field, LINE, Markdown, T } from '../ui';

// Drawn by stui's rules (fixtures/clients/conversation-style.json), so both apps look alike.
const RULES: ConversationRules = rules;
const c = tokenColor;

const empty: Conversation<TimelineEntry> = { entries: [], hasOlder: false, newestSequence: -1 };
type Pending = { id: string; text: string; at: string; failed?: string };
type Row = { kind: 'entry'; entry: ConversationEntry } | { kind: 'pending'; pending: Pending } | { kind: 'older' };

function nowClock() {
  const at = new Date();
  return `${String(at.getHours()).padStart(2, '0')}:${String(at.getMinutes()).padStart(2, '0')}`;
}

// One agent's conversation, drawn as stui draws it and kept pinned to the newest entry just
// above the composer, unless the person has scrolled back to read.
export function ConversationScreen({ route, navigation }: RootScreen<'Conversation'>) {
  const { target, sessionId } = route.params;
  const { data, feed, caps, status, historicalSessions, conversationCache, draftCache, actions } = useStore();
  const session = [...data.sessions, ...historicalSessions].find(candidate => candidate.id === (sessionId ?? target));
  // A running session st manages belongs to its agent: name it, and send to it, as the agent.
  const agent = data.agents.find(candidate => candidate.id === target)
    ?? (session?.state === 'running' && session.managed !== false ? data.agents.find(candidate => candidate.id === session.owner_id) : undefined);
  const unresolved = session ? isUnresolved(session) : false;
  const title = route.params.title ?? (agent ? agentName(agent) : session?.driver ?? target.split('/').pop() ?? target);
  const [timeline, setTimeline] = useState<Conversation<TimelineEntry>>(() => conversationCache.current.get(target) ?? empty);
  // Whether st has answered at all: until it has, the screen says it is loading, never "nothing".
  const [loaded, setLoaded] = useState(() => conversationCache.current.has(target));
  const [issue, setIssue] = useState('');
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  const [pending, setPending] = useState<Pending[]>([]);
  const [find, setFind] = useState('');
  const [findOpen, setFinding] = useState(false);
  const [draft, setDraft] = useState(() => draftCache.current.get(target) ?? '');
  const [away, setAway] = useState(false);
  const list = useRef<FlatList<Row>>(null);
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

  // The socket sends the newest page, then each change; it never polls. Only while visible.
  useFocusEffect(useCallback(() => {
    if (!feed || unresolved) return;
    setIssue('');
    const follow = feed.followConversation(target, {
      onEntries: frame => setTimeline(previous => {
        setLoaded(true);
        const next = applyConversation(previous, frame);
        rememberBounded(conversationCache.current, target, next, 24);
        return next;
      }),
      onIssue: setIssue,
    });
    return () => follow.close();
  }, [feed, target, unresolved, conversationCache]));

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
  const entries = useMemo(() => unreadable ? [] : conversationEntries(timeline.entries, names), [unreadable, timeline.entries, names]);
  // A message st has taken shows up in the conversation; the pending copy then gives way.
  useEffect(() => {
    setPending(previous => previous.filter(item => item.failed || !entries.some(entry => entry.body.kind === 'mail' && entry.body.from === 'you' && entry.body.text.trim() === item.text.trim())));
  }, [entries]);
  const finding = findOpen && find.trim() !== '';
  const found = useMemo(() => finding ? entries.filter(entry => entryMatches(entry, find)) : entries, [entries, find, finding]);
  const rows: Row[] = useMemo(() => [
    ...(finding ? [] : [...pending].reverse().map(item => ({ kind: 'pending' as const, pending: item }))),
    ...[...found].reverse().map(entry => ({ kind: 'entry' as const, entry })),
    ...(timeline.hasOlder && !finding ? [{ kind: 'older' as const }] : []),
  ], [found, pending, timeline.hasOlder, finding]);

  const canSend = !!agent && status === 'online';
  async function send() {
    const text = draft.trim();
    if (!agent || !text) return;
    const item: Pending = { id: `pending-${Date.now()}`, text, at: nowClock() };
    setPending(previous => [...previous, item]);
    setDraft(''); draftCache.current.delete(target);
    list.current?.scrollToOffset({ offset: 0, animated: true });
    const failed = await actions.send(agent.id, text, agent.current_session_id ?? undefined);
    if (failed) setPending(previous => previous.map(candidate => candidate.id === item.id ? { ...candidate, failed } : candidate));
    else setTimeout(() => setPending(previous => previous.filter(candidate => candidate.id !== item.id)), 60_000);
  }
  const toggle = useCallback((id: string) => setOpen(previous => { const next = new Set(previous); if (next.has(id)) next.delete(id); else next.add(id); return next; }), []);

  return <KeyboardAvoidingView style={styles.screen} behavior={Platform.OS === 'ios' ? 'padding' : undefined} keyboardVerticalOffset={headerHeight}>
    <Banners />
    {agent ? <AgentStrip agent={agent} onMission={(id, missionTitle) => navigation.navigate('Mission', { id, title: missionTitle })} /> : session ? <View style={styles.strip}><T dim numberOfLines={1}>{session.driver ?? 'harness'} · {session.state} · {session.id}</T></View> : null}
    {issue ? <View style={styles.strip}><T color={theme.waiting}>{issue}</T></View> : null}
    {findOpen ? <View style={[styles.strip, { flexDirection: 'row', alignItems: 'center', gap: 8 }]}>
      <Field value={find} onChangeText={setFind} placeholder="Find in this conversation" autoFocus autoCapitalize="none" returnKeyType="search" style={{ flex: 1 }} />
      <Pressable accessibilityRole="button" onPress={() => { setFinding(false); setFind(''); }}><T color={theme.accent}>Done</T></Pressable>
    </View> : null}
    {finding ? <View style={styles.strip}><T color={theme.yellow}>{found.length === 0 ? `Nothing here says “${find.trim()}”` : `${found.length} ${found.length === 1 ? 'entry says' : 'entries say'} “${find.trim()}”`}</T></View> : null}
    <FlatList
      ref={list}
      inverted
      data={rows}
      keyExtractor={row => row.kind === 'entry' ? row.entry.id : row.kind === 'pending' ? row.pending.id : 'older'}
      renderItem={({ item: row }) => row.kind === 'older'
        ? <View style={styles.entry}><T dim>older history is not shown here · `st conversations timeline` has all of it</T></View>
        : row.kind === 'pending' ? <PendingView pending={row.pending} />
        // A long press opens the entry's text to select any part of it; iOS text selects only whole.
        : <Pressable onLongPress={() => navigation.navigate('SelectText', { text: entryText(row.entry), title })} delayLongPress={350}><EntryView entry={row.entry} open={open.has(row.entry.id)} onToggle={toggle} /></Pressable>}
      ListEmptyComponent={<View style={[styles.entry, { transform: [{ scaleY: -1 }] }]}>{unreadable ? <T color={theme.waiting} selectable>{unreadable}</T> : <T dim>{unresolved ? 'This process has no exact native session history.' : !loaded ? (status === 'online' ? 'Loading the conversation…' : 'Offline; this conversation has not been loaded yet.') : 'No conversation in the recent timeline.'}</T>}</View>}
      maintainVisibleContentPosition={{ minIndexForVisible: 0, autoscrollToTopThreshold: 60 }}
      keyboardDismissMode="interactive"
      keyboardShouldPersistTaps="handled"
      onScroll={event => setAway(event.nativeEvent.contentOffset.y > 240)}
      scrollEventThrottle={100}
      contentContainerStyle={{ paddingVertical: 8 }}
    />
    {away ? <Pressable accessibilityRole="button" accessibilityLabel="Scroll to latest" style={styles.latest} onPress={() => list.current?.scrollToOffset({ offset: 0, animated: true })}><T bold color={theme.accent}>↓ latest</T></Pressable> : null}
    {agent ? <View style={[styles.composer, { paddingBottom: bottom }]}>
      <T color={theme.accent} style={styles.prompt}>›</T>
      <Field
        value={draft}
        onChangeText={text => { setDraft(text); if (text) rememberBounded(draftCache.current, target, text, 24); else draftCache.current.delete(target); }}
        placeholder={`Message ${title}`}
        multiline
        style={styles.input}
        editable
        accessibilityLabel={`Message ${title}`}
      />
      <Button label="send" disabled={!canSend || !draft.trim()} onPress={() => void send()} style={styles.send} />
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
      <T bold color={color}>{glyph} </T><T bold>{agentName(agent)}</T><T color={color}>  {agentWord(state)}</T>
      <View style={{ flex: 1 }} />
      <T color={harnessColor(harness)}>{harness}</T><T dim> · {agent.host_id?.replace(/^host\//, '') ?? '?'}</T>
    </View>
    <T dim numberOfLines={1}>{agent.id}</T>
    {work ? <Pressable onPress={() => onMission(work.mission_id, work.title || work.path)}><T numberOfLines={1}><T dim>mission </T><T color={theme.accent}>{work.mission_id.replace(/^mission\//, '')} › {work.path}</T></T></Pressable> : null}
  </View>;
}

const PendingView = memo(function PendingView({ pending }: { pending: Pending }) {
  const rule = RULES.pending;
  return <View style={[styles.entry, styles.barred, { borderLeftColor: c(pending.failed ? rule.failed : rule.sending_edge) }]}>
    <T color={c(pending.failed ? rule.failed : rule.sending.color)}>{pending.failed ? `you · not sent: ${pending.failed}` : rule.sending.text}<T dim>  {pending.at}</T></T>
    <Markdown selectable={false} text={pending.text} color={c(rule.text)} />
  </View>;
});

const EntryView = memo(function EntryView({ entry, open, onToggle }: { entry: ConversationEntry; open: boolean; onToggle: (id: string) => void }) {
  const body = entry.body;
  switch (body.kind) {
    case 'user':
      return <View style={[styles.user, { backgroundColor: c(RULES.user.fill) }]}>
        <T color={c(RULES.user.time)} style={{ alignSelf: 'flex-end' }}>{entry.at}</T>
        <T color={c(RULES.user.text)}>{body.text}</T>
      </View>;
    case 'assistant':
      return <View style={styles.entry}><Markdown selectable={false} text={body.text} color={c(RULES.assistant)} /></View>;
    case 'mail': return <MailView entry={entry} body={body} open={open} onToggle={onToggle} />;
    case 'event': {
      const color = body.tone === 'fault' ? theme.red : body.tone === 'warning' ? theme.yellow : c(RULES.event.label);
      return <View style={[styles.entry, { flexDirection: 'row' }]}>
        <T color={c(RULES.event.rule)}>── </T><T color={color} style={{ flex: 1 }}>{body.text} · {entry.at}</T>
      </View>;
    }
    case 'tool': {
      // Quiet until opened, as in stui: an edge in the outcome's colour, dim text, no fill, and
      // at most six one-line rows; opened, it reads at full brightness.
      const rule = RULES.tool;
      const [glyph, color] = body.state === 'running' ? ['⠿', c(rule.running)] : body.state === 'ok' ? [rule.ok.text, c(rule.ok.color)] : [rule.failed.text, c(rule.failed.color)];
      const shown = shownToolLines(body, open);
      const expandable = body.output.length > COLLAPSED_TOOL_LINES;
      const quiet = !open;
      const look = quiet ? rule.quiet : rule.open;
      return <Pressable accessibilityRole={expandable ? 'button' : undefined} accessibilityState={expandable ? { expanded: open } : undefined} disabled={!expandable} onPress={() => onToggle(entry.id)} style={[styles.tool, { borderLeftColor: color }]}>
        <T numberOfLines={1}><T bold color={color}>{glyph} </T><T bold={look.title_bold} color={c(look.title)}>{body.title}</T></T>
        {shown.hidden ? <T color={c(rule.collapse.color)}>  … {shown.hidden} more lines · tap to show</T> : null}
        {open && expandable ? <T color={c(rule.collapse.color)}>  {rule.collapse.text}</T> : null}
        {shown.lines.map((line, index) => <T key={index} numberOfLines={quiet ? 1 : undefined} style={[styles.toolLine, quiet && { opacity: 0.7 }]} color={c(line.startsWith('+') ? rule.added : line.startsWith('-') || line.includes('error') ? rule.removed : look.rows)}>{line || ' '}</T>)}
        {open && expandable ? <T color={c(rule.collapse.color)}>  {rule.collapse.text}</T> : null}
      </Pressable>;
    }
  }
});

type Mail = Extract<ConversationEntry['body'], { kind: 'mail' }>;

// Mail the person is part of leads; mail between others stays back and folds to a few rows,
// like a tool call, until tapped. Mail to the person is filled like their own messages, and
// their own says how far it got: ✓ st has it, ✓✓ the agent has it.
const MailView = memo(function MailView({ entry, body, open, onToggle }: { entry: ConversationEntry; body: Mail; open: boolean; onToggle: (id: string) => void }) {
  const rule = RULES.mail;
  const toYou = body.to === 'you';
  const look = folds(body) ? rule.between_others : rule.involving_you;
  const [height, setHeight] = useState(0);
  const limit = RULES.tool.collapsed_rows * LINE;
  const long = folds(body) && height > limit;
  const mark = body.from !== 'you' ? null : body.delivered ? rule.delivered : rule.sent;
  return <Pressable disabled={!long} accessibilityRole={long ? 'button' : undefined} accessibilityState={long ? { expanded: open } : undefined} onPress={() => onToggle(entry.id)}
    style={[styles.entry, styles.barred, { borderLeftColor: c(look.edge) }, toYou ? { backgroundColor: c(rule.to_you_fill) } : null]}>
    <T><T bold={look.from_bold} color={c(look.from)}>{body.to ? `${body.from} → ${body.to}` : body.from}</T>{body.subject ? <T bold={look.from_bold} color={look.from_bold ? undefined : c(look.text)}>  {body.subject}</T> : null}<T dim>  {entry.at}</T>{mark ? <T color={c(mark.color)}>  {mark.text}</T> : null}</T>
    <View style={long && !open ? { maxHeight: limit, overflow: 'hidden' } : null}>
      {/* Measured at its full height and never shrunk by the fold: a measurement the fold could
          change would fold and unfold the mail in a loop. */}
      <View style={{ flexShrink: 0 }} onLayout={event => { const next = event.nativeEvent.layout.height; setHeight(previous => Math.max(previous, next)); }}><Markdown selectable={false} text={body.text} color={c(look.text)} /></View>
    </View>
    {long ? <T color={c(RULES.tool.collapse.color)}>{open ? `  ${RULES.tool.collapse.text}` : '  … more · tap to show'}</T> : null}
  </Pressable>;
});

const styles = StyleSheet.create({
  screen: { flex: 1, backgroundColor: theme.base },
  strip: { paddingHorizontal: 12, paddingVertical: 6, borderBottomColor: theme.surface0, borderBottomWidth: StyleSheet.hairlineWidth * 2, backgroundColor: theme.base },
  entry: { paddingHorizontal: 12, paddingVertical: 6 },
  user: { backgroundColor: theme.userBg, paddingHorizontal: 12, paddingVertical: 8, marginVertical: 6 },
  barred: { borderLeftWidth: 2, marginLeft: 10, paddingLeft: 8 },
  tool: { marginHorizontal: 8, marginVertical: 4, paddingHorizontal: 8, paddingVertical: 4, borderLeftWidth: 2 },
  toolLine: { fontSize: 12, lineHeight: 17, paddingLeft: 18 },
  composer: { flexDirection: 'row', alignItems: 'flex-end', gap: 6, paddingHorizontal: 10, paddingTop: 6, borderTopColor: theme.surface0, borderTopWidth: StyleSheet.hairlineWidth * 2, backgroundColor: theme.mantle },
  prompt: { paddingBottom: 9, fontFamily: fonts.bold },
  input: { flex: 1, minHeight: 36, maxHeight: 140, marginTop: 0 },
  send: { marginTop: 0, marginBottom: 2 },
  latest: { position: 'absolute', right: 12, bottom: 84, backgroundColor: theme.surface0, paddingHorizontal: 10, paddingVertical: 6, borderRadius: 4 },
});

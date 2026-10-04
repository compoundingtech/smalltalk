import { useEffect, useLayoutEffect, useMemo, useRef } from 'react';
import { Alert, Linking, Pressable, ScrollView, SectionList, View } from 'react-native';
import { useNavigation } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import { Banners, Empty, StatusLine, useDebugScroll, useRefresh } from '../chrome';
import { ContextMenu } from '../menu';
import { HOME_LEGEND, homeRows, homeSections, type HomeRow, cleanMessageText, ANSWERS, isRequest, report, spaced, yesNo } from '@smalltalk/st3-views';
import type { RootParams } from '../navigation';
import { agentName } from '../agentsView';
import { attentionActionLabel, attentionKindLabel } from '../presentation';
import { missionTitle } from '../missionsView';
import { useStore } from '../store';
import { randomName } from '../launcher';
import { theme } from '../theme';
import { Button, Legend, ListRow, Markdown, Note, Screen, SectionHeader, T } from '../ui';
import type { RootScreen } from '../navigation';

// Home: what needs the person, as stui's Home lists it.
export function HomeScreen() {
  const { data, caps, hasSynced, truncated, loadErrors, busy, status, actions } = useStore();
  const navigation = useNavigation<NativeStackNavigationProp<RootParams>>();
  const list = useRef<SectionList<HomeRow>>(null);
  useDebugScroll(list as never);
  const refresh = useRefresh();
  const agentOf = (row: HomeRow) => [row.item.requester_id, row.item.source_id].find(id => id?.startsWith('agent/'));
  const rows = useMemo(() => homeRows(data.attention, caps?.session_actor), [data.attention, caps?.session_actor]);
  // Start something, as stui's launcher does: an agent with its first message, or a shell.
  useLayoutEffect(() => {
    navigation.setOptions({
      unstable_headerRightItems: () => [{
        type: 'menu', label: 'New', icon: { type: 'sfSymbol', name: 'plus' },
        menu: { items: [
          { type: 'action', label: 'New agent', icon: { type: 'sfSymbol', name: 'person.badge.plus' }, onPress: () => navigation.navigate('NewAgent') },
          { type: 'action', label: 'New terminal', icon: { type: 'sfSymbol', name: 'terminal' }, onPress: () => void actions.createTerminal(randomName()).then(terminalId => { if (terminalId) navigation.navigate('Terminal', { terminalId, title: 'shell' }); }) },
          { type: 'action', label: 'New mission', icon: { type: 'sfSymbol', name: 'point.3.connected.trianglepath.dotted' }, onPress: () => navigation.navigate('NewMission') },
        ] },
      }],
    });
  }, [navigation, actions]);
  const sections = useMemo(() => homeSections(rows).map(section => ({ ...section, data: section.rows })), [rows]);
  // Dismiss, as stui's x: a request is closed with word to its asker that there is nothing for
  // the person to do (after a confirm), an update or a message is read. A decision has none.
  const dismiss = (row: HomeRow): (() => void) | undefined => {
    const item = row.item;
    if (item.update && item.actions.includes('work.done')) return () => void actions.done(item, 'Read', 'read');
    if (isRequest(item.attention_kind) && item.actions.includes('work.done')) {
      const asker = item.requester_id?.replace(/^agent\//, '') ?? 'the agent';
      return () => Alert.alert(`Tell ${asker} there is nothing for you to do`, row.title, [
        { text: 'Cancel', style: 'cancel' },
        { text: 'Dismiss', onPress: () => void actions.done(item, ANSWERS.nothing) },
      ]);
    }
    if (item.actions.includes('message.read')) return () => void actions.markRead(item);
    return undefined;
  };
  return <Screen>
    <Banners />
    <SectionList
      ref={list}
      contentInsetAdjustmentBehavior="automatic"
      sections={sections}
      keyExtractor={row => row.item.id}
      stickySectionHeadersEnabled={false}
      renderSectionHeader={({ section }) => <SectionHeader title={section.title} count={section.count} color={section.tier === 'stopped' ? theme.person : theme.overlay1} />}
      renderItem={({ item: row }) => <ContextMenu title={row.title} actions={[
        { id: 'open', title: 'Open', symbol: 'arrow.up.right', run: () => navigation.navigate('Attention', { id: row.item.id }) },
        ...(agentOf(row) ? [{ id: 'chat', title: 'Chat with the agent', symbol: 'bubble.left.and.bubble.right', run: () => navigation.navigate('Conversation', { target: agentOf(row)! }) }] : []),
        ...(row.item.mission_id ? [{ id: 'mission', title: 'Open the mission', symbol: 'point.3.connected.trianglepath.dotted', run: () => navigation.navigate('Mission', { id: row.item.mission_id! }) }] : []),
        ...(row.item.actions.includes('work.done') && !row.item.update && !busy && status === 'online' ? [{ id: 'done', title: 'Complete step', symbol: 'checkmark.circle', run: () => Alert.prompt('Complete step', row.title, summary => { if (summary.trim()) void actions.done(row.item, summary); }) }] : []),
        ...(dismiss(row) && !busy && status === 'online' ? [{ id: 'dismiss', title: 'Dismiss', symbol: 'xmark.circle', run: dismiss(row)! }] : []),
      ]}>
        <ListRow
          glyph={row.glyph}
          glyphColor={theme[row.color]}
          title={<T numberOfLines={1}><T color={theme[row.color]}>{row.kind.padEnd(9)}</T><T bold>{row.title}</T></T>}
          second={`${row.waiting ? `${row.waiting} · ` : ''}waited ${row.age}`}
          onPress={() => navigation.navigate('Attention', { id: row.item.id })}
        />
      </ContextMenu>}
      ListHeaderComponent={<StatusLine />}
      refreshControl={refresh}
      ListEmptyComponent={<Empty text={loadErrors.attention ? `Attention could not be loaded: ${loadErrors.attention}` : hasSynced ? 'Nothing needs you.' : 'Checking what needs you…'} />}
      ListFooterComponent={<View>
        {truncated.attention ? <Note tone="warning">More items exist beyond these 200. `st now` lists them all.</Note> : null}
        <Legend entries={HOME_LEGEND.map(entry => ({ ...entry, color: theme[entry.color] }))} />
      </View>}
      style={{ flex: 1 }}
    />
  </Screen>;
}

// One attention item: what it asks, who raised it, and what can be done here.
export function AttentionScreen({ route, navigation }: RootScreen<'Attention'>) {
  const { data, busy, status, actions } = useStore();
  const found = data.attention.find(candidate => candidate.id === route.params.id);
  // An update clears once read, which is as soon as it is opened; it stays here while it is read.
  const kept = useRef(found);
  if (found) kept.current = found;
  const item = found ?? (kept.current?.update ? kept.current : undefined);
  const read = useRef(false);
  useEffect(() => {
    if (!item?.update || read.current || status !== 'online') return;
    read.current = true;
    void actions.done(item, 'Read', 'read');
  }, [item?.id, status]);
  if (!item) return <Screen><Banners /><Empty text="This item is no longer open." /></Screen>;
  const [row] = homeRows([item], undefined);
  const agentId = [item.requester_id, item.source_id].find(id => id?.startsWith('agent/'));
  const agent = agentId ? data.agents.find(candidate => candidate.id === agentId) : undefined;
  const mission = item.mission_id ? data.missions.find(candidate => candidate.id === item.mission_id) : undefined;
  const other = item.actions.filter(action => action !== 'work.done' && action !== 'message.read');
  if (item.update) {
    const from = agent ? agentName(agent) : item.requester_id?.replace(/^agent\//, '') ?? 'An agent';
    return <Screen>
      <Banners />
      <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, gap: 8, paddingBottom: 32 }}>
        <T><T bold color={theme[row.color]}>{row.glyph} update</T><T dim>  {row.age} ago</T></T>
        <T bold selectable>{row.title}</T>
        <T><T dim>from  </T><T bold color={theme.person}>{from}</T></T>
        <Markdown text={spaced(cleanMessageText(item.detail || item.update.summary || ''))} color={theme.text} />
        <T dim selectable>about {item.update.about}</T>
        {item.update.subjects?.map((subject, index) => subject.url
          ? <Pressable key={index} onPress={() => void Linking.openURL(subject.url!)}><T color={theme.accent}>↗ {subject.label}</T></Pressable>
          : <T key={index} dim>↗ {subject.label}  {subject.ref ?? ''}</T>)}
        <T dim>Nothing waits on this. Opening it marked it read, so it has left Home.</T>
        {agentId ? <Button label={`chat with ${from}`} onPress={() => navigation.navigate('Conversation', { target: agentId, title: from })} /> : null}
        <T dim selectable>{item.id}</T>
      </ScrollView>
    </Screen>;
  }
  if (isRequest(item.attention_kind) && item.actions.includes('work.done')) {
    const from = agent ? agentName(agent) : item.requester_id?.replace(/^agent\//, '') ?? 'An agent';
    return <Screen>
      <Banners />
      {/* Automatic insets keep the end of a long request clear of the floating tab bar. */}
      <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, gap: 8, paddingBottom: 32 }}>
        <T><T bold color={theme[row.color]}>{row.glyph} request</T><T dim>  {item.priority} · waited {row.age}</T></T>
        <T bold selectable>{row.title}</T>
        <T><T dim>asks  </T><T bold color={theme.person}>{from}</T></T>
        {item.request ? <StructuredRequestView item={item} request={item.request} from={from} onAnswered={() => navigation.goBack()} /> : <>
          <RequestQuestion text={cleanMessageText(item.detail ?? '')} />
          <T bold color={theme.person}>{from} is waiting on you.</T>
          <RequestAnswers item={item} from={from} onAnswered={() => navigation.goBack()} />
        </>}
        <T dim>Answer it, or Nothing to do if there is nothing for you to do. Either way the step it waits on continues.</T>
        {mission ? <Button label={`mission ${missionTitle(mission)}`} onPress={() => navigation.navigate('Mission', { id: mission.id })} /> : null}
        {agentId ? <Button label={`chat with ${from}`} onPress={() => navigation.navigate('Conversation', { target: agentId, title: from })} /> : null}
        <T dim selectable>{item.id}</T>
      </ScrollView>
    </Screen>;
  }
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, gap: 6, paddingBottom: 32 }}>
      <T><T bold color={theme[row.color]}>{row.glyph} {row.kind}</T><T dim>  {attentionKindLabel(item.attention_kind)} · {item.priority} · waited {row.age}</T></T>
      <T bold selectable>{row.title}</T>
      {item.detail ? <Markdown text={item.detail} color={theme.subtext0} /> : null}
      {item.because ? <T soft>because {item.because}</T> : null}
      {row.waiting ? <T dim>{row.waiting}</T> : null}
      {mission ? <Button label={`mission ${missionTitle(mission)}`} onPress={() => navigation.navigate('Mission', { id: mission.id })} /> : item.mission_id ? <T dim>mission {item.mission_id}</T> : null}
      {agentId ? <Button label={`chat with ${agent ? agentName(agent) : agentId}`} onPress={() => navigation.navigate('Conversation', { target: agentId, title: agent ? agentName(agent) : undefined })} /> : null}
      <T dim selectable>{item.id}{item.source_id !== item.id ? ` · from ${item.source_id}` : ''}</T>
      {item.actions.includes('work.done') ? <Button label={attentionActionLabel('work.done').toLowerCase()} disabled={busy || status !== 'online'} onPress={() => Alert.prompt('Complete step', item.title, summary => { if (summary.trim()) void actions.done(item, summary).then(done => { if (done) navigation.goBack(); }); })} /> : null}
      {item.actions.includes('message.read') ? <Button label="dismiss: mark read" disabled={busy || status !== 'online'} onPress={() => void actions.markRead(item).then(done => { if (done) navigation.goBack(); })} /> : null}
      {other.length ? <Note>in the CLI: {other.map(attentionActionLabel).join(', ')}</Note> : null}
    </ScrollView>
  </Screen>;
}

// A structured request (#1010) as stui shows it: the recommendation first, then the question, its
// summary, reasons and links, and each named answer with what it does. A tap on an answer asks
// to confirm, then sends it by its id.
type Structured = NonNullable<Parameters<ReturnType<typeof useStore>['actions']['done']>[0]['request']>;
function StructuredRequestView({ item, request, from, onAnswered }: { item: Parameters<ReturnType<typeof useStore>['actions']['done']>[0]; request: Structured; from: string; onAnswered: () => void }) {
  const { busy, status, actions } = useStore();
  const disabled = busy || status !== 'online';
  const label = (id: string) => request.answers?.find(answer => answer.id === id)?.label ?? id;
  const send = (answer: { id: string; label: string; outcome?: string | null }) => answer.outcome === 'request_changes'
    // Requesting changes needs the changes, in words.
    ? Alert.prompt(answer.label, 'What should change?', text => { if (text.trim()) void actions.done(item, text.trim(), answer.id).then(done => { if (done) onAnswered(); }); })
    : Alert.alert(`Answer ${from} “${answer.label}”`, undefined, [
    { text: 'Cancel', style: 'cancel' },
    { text: 'Send', onPress: () => void actions.done(item, answer.label, answer.id).then(done => { if (done) onAnswered(); }) },
  ]);
  const words = () => Alert.prompt(`Answer ${from}`, item.title, text => { if (text.trim()) void actions.done(item, text.trim()).then(done => { if (done) onAnswered(); }); });
  return <View style={{ gap: 8 }}>
    {request.recommendation ? <T><T dim>recommends  </T><T bold color={theme.green}>{label(request.recommendation.answer)}</T><T soft>  {request.recommendation.reason}</T></T> : null}
    <RequestQuestion text={request.question} />
    {request.summary ? <Markdown text={spaced(request.summary)} color={theme.subtext0} /> : null}
    {request.reasons?.length ? <View style={{ gap: 2 }}>{request.reasons.map((reason, index) => <T key={index}><T color={theme.lavender}>•  </T>{reason}</T>)}</View> : null}
    {request.subjects?.map((subject, index) => subject.url
      ? <Pressable key={index} onPress={() => void Linking.openURL(subject.url!)}><T color={theme.accent}>↗ {subject.label}</T></Pressable>
      : <T key={index} dim>↗ {subject.label}  {subject.ref ?? ''}</T>)}
    <T bold color={theme.person}>{from} is waiting on you.</T>
    {request.answers?.map(answer => <Pressable key={answer.id} disabled={disabled} onPress={() => send(answer)}
      style={{ borderLeftWidth: 2, borderLeftColor: request.recommendation?.answer === answer.id ? theme.green : theme.surface1, paddingLeft: 8, paddingVertical: 4, opacity: disabled ? 0.5 : 1 }}>
      <T bold color={theme.accent}>{answer.label}{request.recommendation?.answer === answer.id ? <T color={theme.green}>  recommended</T> : null}</T>
      <T dim>{answer.consequence}</T>
    </Pressable>)}
    {request.custom ? <Button label="Answer in words" disabled={disabled} onPress={words} /> : null}
    {request.why_person ? <T dim>Why you: {request.why_person}</T> : null}
  </View>;
}

// A request's question: a JSON report as its telling fields, anything else as Markdown.
function RequestQuestion({ text }: { text: string }) {
  const shown = report(text);
  if (!shown) return <Markdown text={spaced(text)} color={theme.text} />;
  const tone = { fault: theme.red, text: theme.text, soft: theme.subtext0 } as const;
  return <View style={{ gap: 2 }}>
    {shown.before ? <Markdown text={shown.before} color={theme.text} /> : null}
    {shown.rows.map(row => <T key={row.key} numberOfLines={1}><T dim>{row.key}  </T><T color={tone[row.tone]}>{row.value}</T></T>)}
    {shown.more ? <T dim>… {shown.more} more fields</T> : null}
  </View>;
}

// stui's answers: Yes and No for a yes-or-no question, words, or Nothing to do. Each completes
// the waiting step with a short reason, after the person confirms it.
function RequestAnswers({ item, from, onAnswered }: { item: Parameters<ReturnType<typeof useStore>['actions']['done']>[0]; from: string; onAnswered: () => void }) {
  const { busy, status, actions } = useStore();
  const disabled = busy || status !== 'online';
  const send = (reason: string) => void actions.done(item, reason).then(done => { if (done) onAnswered(); });
  const confirm = (label: string, reason: string) => Alert.alert(label, undefined, [{ text: 'Cancel', style: 'cancel' }, { text: 'Send', onPress: () => send(reason) }]);
  const words = () => Alert.prompt(`Answer ${from}`, item.title, text => { if (text.trim()) send(text.trim()); });
  return <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 8 }}>
    {yesNo(item.title) ? <>
      <Button label="Yes" color={theme.green} disabled={disabled} onPress={() => confirm(`Answer ${from} “Yes”`, ANSWERS.yes)} />
      <Button label="No" color={theme.red} disabled={disabled} onPress={() => confirm(`Answer ${from} “No”`, ANSWERS.no)} />
      <Button label="Answer in words" disabled={disabled} onPress={words} />
    </> : <Button label="Answer" disabled={disabled} onPress={words} />}
    <Button label="Nothing to do" color={theme.overlay1} disabled={disabled} onPress={() => confirm(`Tell ${from} there is nothing for you to do`, ANSWERS.nothing)} />
  </View>;
}

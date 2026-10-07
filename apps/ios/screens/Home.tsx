import { useEffect, useLayoutEffect, useMemo, useRef } from 'react';
import { Alert, Linking, Pressable, ScrollView, SectionList, View } from 'react-native';
import { useNavigation } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import { Banners, Empty, StatusLine, useDebugScroll, useMissionsOnFocus, useRefresh } from '../chrome';
import { ContextMenu } from '../menu';
import { HOME_LEGEND, alertsHeading, homeRows, homeSections, openAlerts, type HomeRow, cleanMessageText, ANSWERS, isRequest, report, spaced, yesNo } from '@smalltalk/st3-views';
import type { RootParams } from '../navigation';
import { agentName } from '../agentsView';
import { attentionActionLabel, attentionKindLabel, blockedLine } from '../presentation';
import { missionTitle } from '../missionsView';
import { useStore } from '../store';
import { randomName } from '../launcher';
import { AlertBody, RequestAnswers, RequestQuestion, StructuredRequestView } from './Alerts';
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
  // Start something, as stui's launcher does: a shell or a mission.
  useLayoutEffect(() => {
    navigation.setOptions({
      unstable_headerLeftItems: () => [{ type: 'button', label: 'Resources', icon: { type: 'sfSymbol', name: 'sidebar.left' }, onPress: () => navigation.navigate('Sidebar') }],
      unstable_headerRightItems: () => [{
        type: 'menu', label: 'New', icon: { type: 'sfSymbol', name: 'plus' },
        menu: { items: [
          { type: 'action', label: 'New terminal', icon: { type: 'sfSymbol', name: 'terminal' }, onPress: () => void actions.createTerminal(randomName()).then(terminalId => { if (terminalId) navigation.navigate('Terminal', { terminalId, title: 'shell' }); }) },
          { type: 'action', label: 'New mission', icon: { type: 'sfSymbol', name: 'point.3.connected.trianglepath.dotted' }, onPress: () => navigation.navigate('NewMission') },
        ] },
      }],
    });
  }, [navigation, actions]);
  // The count of what waits on the person, as the other clients print it: nothing at zero.
  const heading = alertsHeading(openAlerts(data.attention, caps?.session_actor).length);
  const sections = useMemo(() => homeSections(rows).map(section => ({ ...section, data: section.rows })), [rows]);
  // Dismiss, as stui's x: a request is closed with word to its asker that there is nothing for
  // the person to do (after a confirm), an update or a message is read. A decision has none.
  const dismiss = (row: HomeRow): (() => void) | undefined => {
    const item = row.item;
    // Closed elsewhere: only clearing it takes it off Home.
    if (item.closedElsewhere) return () => actions.clearClosed(item.id);
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
      ListHeaderComponent={<View><StatusLine />{heading ? <T bold color={theme.person} style={{ paddingHorizontal: 12, paddingTop: 4 }}>◆ {heading}</T> : null}</View>}
      refreshControl={refresh}
      ListEmptyComponent={loadErrors.attention ? <Empty text={`Alerts could not be loaded: ${loadErrors.attention}`} /> : hasSynced ? null : <Empty text="Checking for alerts…" />}
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
  useMissionsOnFocus();
  const found = data.attention.find(candidate => candidate.id === route.params.id);
  // Nothing leaves Home by itself: an update is read when the person says so, never by opening it.
  const kept = useRef(found);
  if (found) kept.current = found;
  const item = found ?? kept.current;
  if (!item) return <Screen><Banners /><Empty text="This item is no longer open." /></Screen>;
  const [row] = homeRows([item], undefined);
  const agentId = [item.requester_id, item.source_id].find(id => id?.startsWith('agent/'));
  const agent = agentId ? data.agents.find(candidate => candidate.id === agentId) : undefined;
  const mission = item.mission_id ? data.missions.find(candidate => candidate.id === item.mission_id) : undefined;
  const other = item.actions.filter(action => action !== 'work.done' && action !== 'message.read' && action !== 'prompt.respond');
  // Closed elsewhere, so nothing here can be answered; it stays until the person clears it.
  if (item.closedElsewhere && !item.update) {
    return <Screen>
      <Banners />
      <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, gap: 6, paddingBottom: 32 }}>
        <T><T bold color={theme[row.color]}>{row.glyph} {row.kind}</T><T dim>  {row.age} ago</T></T>
        <T bold selectable>{row.title}</T>
        {item.detail ? <Markdown text={item.detail} color={theme.subtext0} /> : null}
        <T dim>This was closed. It stays under Recently closed until you clear it.</T>
        <Button label="clear from Home" onPress={() => { actions.clearClosed(item.id); navigation.goBack(); }} />
        <T dim selectable>{item.id}</T>
      </ScrollView>
    </Screen>;
  }
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
        <T dim>{item.closedElsewhere ? 'This was closed. It stays under Recently closed until you clear it.' : 'Nothing waits on this. It stays on Home until you mark it read.'}</T>
        {item.closedElsewhere
          ? <Button label="clear from Home" onPress={() => { actions.clearClosed(item.id); navigation.goBack(); }} />
          : item.actions.includes('work.done') ? <Button label="mark read" disabled={busy || status !== 'online'} onPress={() => void actions.done(item, 'Read', 'read').then(done => { if (done) navigation.goBack(); })} /> : null}
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
        {blockedLine(item) ? <T dim selectable>{blockedLine(item)}</T> : <T dim>Answer it, or Nothing to do if there is nothing for you to do. Either way the step it waits on continues.</T>}
        {mission ? <Button label={`mission ${missionTitle(mission)}`} onPress={() => navigation.navigate('Mission', { id: mission.id })} /> : item.mission_id ? <T dim>mission {item.mission_id}</T> : null}
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
      {item.attention_kind === 'harness-prompt' || item.attention_kind === 'harness-login'
        // A prompt is allowed from its conversation, where the call it asks about is shown.
        ? <AlertBody item={item} from={agent ? agentName(agent) : item.source_id} calls={[]} seats={[]} />
        : item.detail ? <Markdown text={item.detail} color={theme.subtext0} /> : null}
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

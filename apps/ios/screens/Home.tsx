import { useLayoutEffect, useMemo, useRef } from 'react';
import { Alert, SectionList, View } from 'react-native';
import { useNavigation } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import { Banners, Empty, StatusLine, useDebugScroll, useRefresh } from '../chrome';
import { ContextMenu } from '../menu';
import { HOME_LEGEND, homeRows, homeSections, type HomeRow } from '../homeView';
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
        ...(row.item.actions.includes('work.done') && !busy && status === 'online' ? [{ id: 'done', title: 'Complete step', symbol: 'checkmark.circle', run: () => Alert.prompt('Complete step', row.title, summary => { if (summary.trim()) void actions.done(row.item, summary); }) }] : []),
      ]}>
        <ListRow
          glyph={row.glyph}
          glyphColor={row.color}
          title={<T numberOfLines={1}><T color={row.color}>{row.kind.padEnd(9)}</T><T bold>{row.title}</T></T>}
          second={`${row.waiting ? `${row.waiting} · ` : ''}waited ${row.age}`}
          onPress={() => navigation.navigate('Attention', { id: row.item.id })}
        />
      </ContextMenu>}
      ListHeaderComponent={<StatusLine />}
      refreshControl={refresh}
      ListEmptyComponent={<Empty text={loadErrors.attention ? `Attention could not be loaded: ${loadErrors.attention}` : hasSynced ? 'Nothing needs you.' : 'Checking what needs you…'} />}
      ListFooterComponent={<View>
        {truncated.attention ? <Note tone="warning">More items exist beyond these 200. `st now` lists them all.</Note> : null}
        <Legend entries={HOME_LEGEND} />
      </View>}
      style={{ flex: 1 }}
    />
  </Screen>;
}

// One attention item: what it asks, who raised it, and what can be done here.
export function AttentionScreen({ route, navigation }: RootScreen<'Attention'>) {
  const { data, busy, status, actions } = useStore();
  const item = data.attention.find(candidate => candidate.id === route.params.id);
  if (!item) return <Screen><Banners /><Empty text="This item is no longer open." /></Screen>;
  const [row] = homeRows([item], undefined);
  const agentId = [item.requester_id, item.source_id].find(id => id?.startsWith('agent/'));
  const agent = agentId ? data.agents.find(candidate => candidate.id === agentId) : undefined;
  const mission = item.mission_id ? data.missions.find(candidate => candidate.id === item.mission_id) : undefined;
  const other = item.actions.filter(action => action !== 'work.done');
  return <Screen>
    <Banners />
    <View style={{ padding: 12, gap: 6 }}>
      <T><T bold color={row.color}>{row.glyph} {row.kind}</T><T dim>  {attentionKindLabel(item.attention_kind)} · {item.priority} · waited {row.age}</T></T>
      <T bold selectable>{row.title}</T>
      {item.detail ? <Markdown text={item.detail} color={theme.subtext0} /> : null}
      {item.because ? <T soft>because {item.because}</T> : null}
      {row.waiting ? <T dim>{row.waiting}</T> : null}
      {mission ? <Button label={`mission ${missionTitle(mission)}`} onPress={() => navigation.navigate('Mission', { id: mission.id })} /> : item.mission_id ? <T dim>mission {item.mission_id}</T> : null}
      {agentId ? <Button label={`chat with ${agent ? agentName(agent) : agentId}`} onPress={() => navigation.navigate('Conversation', { target: agentId, title: agent ? agentName(agent) : undefined })} /> : null}
      <T dim selectable>{item.id}{item.source_id !== item.id ? ` · from ${item.source_id}` : ''}</T>
      {item.actions.includes('work.done') ? <Button label={attentionActionLabel('work.done').toLowerCase()} disabled={busy || status !== 'online'} onPress={() => Alert.prompt('Complete step', item.title, summary => { if (summary.trim()) void actions.done(item, summary).then(done => { if (done) navigation.goBack(); }); })} /> : null}
      {other.length ? <Note>in the CLI: {other.map(attentionActionLabel).join(', ')}</Note> : null}
    </View>
  </Screen>;
}

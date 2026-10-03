import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { FlatList, SectionList, View } from 'react-native';
import { useNavigation } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import SegmentedControl from '@react-native-segmented-control/segmented-control';
import { saidRows, type SaidRow } from '../conversationSearch';
import { AGENT_LEGEND, agentGlyph, agentRows, agentSections, agentTreeLines, filterAgentRows, harnessColor, UNMANAGED_GROUP, type AgentRowView, type TreeLine } from '../agentsView';
import { Banners, Empty, StatusLine, useDebugScroll, useListsOnFocus, useRefresh } from '../chrome';
import { ContextMenu, type MenuAction } from '../menu';
import type { RootParams } from '../navigation';
import { sessionDetail } from '../sessionView';
import { useStore } from '../store';
import { theme } from '../theme';
import { Button, Legend, ListRow, Note, Screen, SectionHeader, T } from '../ui';

const legend = AGENT_LEGEND.map(entry => ({ ...agentGlyph(entry.state), word: entry.word }));

function Right({ row }: { row: AgentRowView }) {
  return <><T color={harnessColor(row.harness)}>{row.harness}</T><T dim>{row.activity.padStart(4)}</T></>;
}

// Agents: every seat, grouped and sorted as stui's Agents tab, or as a tree of their paths. The
// navigation bar's native search field filters both.
export function AgentsScreen() {
  const { data, gatewayHost, truncated, hasSynced, loadErrors, treeView, setTreeView, status, actions } = useStore();
  const navigation = useNavigation<NativeStackNavigationProp<RootParams>>();
  useListsOnFocus(['sessions']);
  const refresh = useRefresh(['sessions']);
  const list = useRef<SectionList<AgentRowView>>(null), tree = useRef<FlatList<TreeLine>>(null);
  useDebugScroll(list as never);
  useDebugScroll(tree as never);
  const [filter, setFilter] = useState('');
  useLayoutEffect(() => {
    navigation.setOptions({
      headerSearchBarOptions: {
        placeholder: 'Filter agents',
        hideWhenScrolling: true,
        autoCapitalize: 'none',
        onChangeText: event => setFilter(event.nativeEvent.text),
        onCancelButtonPress: () => setFilter(''),
      },
    });
  }, [navigation]);
  // Typed text also searches what was said in conversations, as stui's Ctrl+K does.
  const [said, setSaid] = useState<{ query: string; rows: SaidRow[]; note: string } | null>(null);
  const query = filter.trim();
  useEffect(() => {
    if (query.length < 3) { setSaid(null); return; }
    let live = true;
    const timer = setTimeout(() => void actions.searchConversations(query).then(found => {
      if (!live) return;
      setSaid(typeof found === 'string' ? { query, rows: [], note: `search failed: ${found}` } : { query, ...saidRows(found, data.agents) });
    }), 250);
    return () => { live = false; clearTimeout(timer); };
  }, [query, status]); // eslint-disable-line react-hooks/exhaustive-deps
  const rows = useMemo(() => filterAgentRows(agentRows(data.agents, data.sessions, gatewayHost || '?'), filter), [data.agents, data.sessions, gatewayHost, filter]);
  const open = (row: AgentRowView) => navigation.navigate('Conversation', { target: row.target, ...(row.unmanaged ? { sessionId: row.id } : {}), title: row.name });
  const menu = (row: AgentRowView): MenuAction[] => {
    const agent = data.agents.find(candidate => candidate.id === row.id);
    const runtime = agent && agent.runtime_ids.length && !['stopped', 'failed'].includes(agent.state) ? agent.runtime_ids[0] : undefined;
    const work = agent?.current_work?.[0];
    return [
      { id: 'chat', title: 'Open conversation', symbol: 'bubble.left.and.bubble.right', run: () => open(row) },
      ...(runtime && status === 'online' ? [{ id: 'terminal', title: 'Open terminal', symbol: 'terminal', run: () => void actions.runtimeTerminal(runtime).then(terminalId => { if (terminalId) navigation.navigate('Terminal', { terminalId, title: row.name }); }) }] : []),
      ...(work ? [{ id: 'mission', title: `Open ${work.path.split('/').pop()}`, symbol: 'point.3.connected.trianglepath.dotted', run: () => navigation.navigate('Mission', { id: work.mission_id }) }] : []),
    ];
  };
  const header = <View>
    <StatusLine />
    <View style={{ paddingHorizontal: 12, paddingTop: 8 }}>
      <SegmentedControl values={['List', 'Tree']} selectedIndex={treeView ? 1 : 0} onChange={event => setTreeView(event.nativeEvent.selectedSegmentIndex === 1)} appearance="dark" accessibilityLabel="Show agents as a list or a tree" />
    </View>
  </View>;
  const footer = <View style={{ paddingBottom: 24 }}>
    {said && said.query === query && (said.rows.length || said.note) ? <View>
      <SectionHeader title="said in conversations" count={said.rows.length} color={theme.overlay1} />
      {said.note ? <Note tone="warning">{said.note}</Note> : null}
      {said.rows.map(hit => <ListRow
        key={hit.key}
        glyph="›"
        glyphColor={theme.overlay1}
        title={hit.excerpt}
        right={<T dim>{hit.when}</T>}
        second={hit.name}
        onPress={() => navigation.navigate('Conversation', { target: hit.agentId, title: hit.name, find: said.query })}
        accessibilityLabel={`${hit.name} said ${hit.excerpt}`}
      />)}
    </View> : null}
    {truncated.agents ? <Note tone="warning">More agents exist beyond these 200.</Note> : null}
    <Legend entries={legend} />
    <View style={{ paddingHorizontal: 12 }}><Button label="past sessions" onPress={() => navigation.navigate('History')} /></View>
  </View>;
  const empty = <Empty text={filter ? `No agent matches “${filter}”.` : loadErrors.agents ? `Agents could not be loaded: ${loadErrors.agents}` : hasSynced ? 'No agents yet.' : 'Loading agents…'} />;
  const row = (item: AgentRowView, indent = 0, second?: string) => <ContextMenu title={item.name} actions={menu(item)}>
    <ListRow
      indent={indent}
      glyph={agentGlyph(item.state).glyph}
      glyphColor={agentGlyph(item.state).color}
      title={item.name}
      right={<Right row={item} />}
      second={second}
      onPress={() => open(item)}
      accessibilityLabel={`${item.name}, ${item.unmanaged ? UNMANAGED_GROUP : item.state}, ${item.harness}`}
    />
  </ContextMenu>;
  if (treeView) {
    return <Screen>
      <Banners />
      <FlatList
        ref={tree}
        contentInsetAdjustmentBehavior="automatic"
        keyboardDismissMode="on-drag"
        data={agentTreeLines(rows)}
        keyExtractor={line => line.key}
        renderItem={({ item: line }) => line.kind === 'folder'
          ? <View style={{ paddingLeft: 12 + line.depth * 16, paddingVertical: 2 }}><T><T color={theme.surface2}>▾ </T><T color={theme.overlay1}>{line.name}/</T></T></View>
          : row(line.row, line.depth)}
        ListHeaderComponent={header}
        ListEmptyComponent={empty}
        ListFooterComponent={footer}
        refreshControl={refresh}
      />
    </Screen>;
  }
  const sections = agentSections(rows).map(section => ({ ...section, data: section.rows }));
  return <Screen>
    <Banners />
    <SectionList
      ref={list}
      contentInsetAdjustmentBehavior="automatic"
      keyboardDismissMode="on-drag"
      sections={sections}
      keyExtractor={item => item.id}
      stickySectionHeadersEnabled={false}
      renderSectionHeader={({ section }) => <SectionHeader title={section.title} count={section.count} color={section.person ? theme.person : theme.overlay1} />}
      renderItem={({ item }) => row(item, 0, item.path)}
      ListHeaderComponent={header}
      ListEmptyComponent={empty}
      ListFooterComponent={footer}
      refreshControl={refresh}
    />
  </Screen>;
}

// Past sessions, read when the person asks for them.
export function HistoryScreen() {
  const { historicalSessions, actions, status } = useStore();
  const navigation = useNavigation<NativeStackNavigationProp<RootParams>>();
  useEffect(() => { if (status === 'online') void actions.history(); }, [status]); // eslint-disable-line react-hooks/exhaustive-deps
  return <Screen>
    <Banners />
    <FlatList
      contentInsetAdjustmentBehavior="automatic"
      data={historicalSessions}
      keyExtractor={session => session.id}
      renderItem={({ item: session }) => <ListRow glyph={session.state === 'failed' ? '✕' : '✓'} glyphColor={session.state === 'failed' ? theme.fault : theme.done} title={session.owner_id.replace(/^agent\//, '')} second={sessionDetail(session)} onPress={() => navigation.navigate('Conversation', { target: session.id, sessionId: session.id, title: session.owner_id.split('/').pop() })} />}
      ListEmptyComponent={<Empty text={status === 'online' ? 'No past sessions.' : 'Past sessions load when connected.'} />}
    />
  </Screen>;
}

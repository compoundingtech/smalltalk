import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { Alert, Pressable, ScrollView, SectionList, View } from 'react-native';
import { useNavigation } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import type { LaunchVariant, Mission } from '../../../clients/typescript/st3-client';
import { agentName } from '../agentsView';
import SegmentedControl from '@react-native-segmented-control/segmented-control';
import { Banners, Empty, StatusLine, useDebugScroll, useListsOnFocus, useRefresh } from '../chrome';
import { MISSION_LEGEND, missionRows, missionSections, missionTitle, missionWord, stepStyle, wordName, wordStyle, type MissionRow } from '../missionsView';
import type { RootParams, RootScreen } from '../navigation';
import { missionSteps } from '../presentation';
import { homeRows } from '@smalltalk/st3-views';
import { useStore, type Planner } from '../store';
import { theme } from '../theme';
import { Button, Field, Legend, ListRow, Markdown, Note, Screen, SectionHeader, T } from '../ui';

const legend = MISSION_LEGEND.map(word => ({ ...wordStyle(word), word: wordName(word) }));

function Meter({ done, total, color }: { done: number; total: number; color: string }) {
  // st reports only current steps for many runs, so a 0/1 meter would claim too much.
  if (!done) return <T dim>{total} step{total === 1 ? '' : 's'}</T>;
  const filled = Math.round((done / Math.max(total, 1)) * 5);
  return <T><T color={color}>{'▰'.repeat(filled)}</T><T color={theme.surface2}>{'▱'.repeat(5 - filled)}</T><T dim> {done}/{total}</T></T>;
}

// Missions: one word per mission for who has to move, in stui's order.
export function MissionsScreen() {
  const { data, truncated, hasSynced, loadErrors } = useStore();
  const navigation = useNavigation<NativeStackNavigationProp<RootParams>>();
  useListsOnFocus(['launches']);
  const refresh = useRefresh(['launches']);
  const [showSystem, setShowSystem] = useState(false);
  const list = useRef<SectionList<MissionRow>>(null);
  useDebugScroll(list as never);
  // Native bar buttons: a menu for what the list includes, and a new mission.
  useLayoutEffect(() => {
    navigation.setOptions({
      unstable_headerRightItems: () => [
        { type: 'menu', label: 'Options', icon: { type: 'sfSymbol', name: 'line.3.horizontal.decrease.circle' }, menu: { items: [{ type: 'action', label: 'Show system missions', state: showSystem ? 'on' : 'off', onPress: () => setShowSystem(value => !value) }] } },
        { type: 'button', label: 'New mission', icon: { type: 'sfSymbol', name: 'plus' }, onPress: () => navigation.navigate('NewMission') },
      ],
    });
  }, [navigation, showSystem]);
  const { rows, hidden } = useMemo(() => missionRows(data.missions, data.attention, data.agents, showSystem), [data.missions, data.attention, data.agents, showSystem]);
  const sections = missionSections(rows).map(section => ({ ...section, data: section.rows }));
  const open = data.launches.filter(launch => launch.phase !== 'approved' && launch.phase !== 'cancelled');
  return <Screen>
    <Banners />
    <SectionList
      ref={list}
      contentInsetAdjustmentBehavior="automatic"
      refreshControl={refresh}
      ListHeaderComponent={<StatusLine />}
      sections={sections}
      keyExtractor={row => row.mission.id}
      stickySectionHeadersEnabled={false}
      renderSectionHeader={({ section }) => <SectionHeader title={section.title} count={section.count} color={wordStyle(section.word).color} />}
      renderItem={({ item: row }) => <ListRow
        glyph={wordStyle(row.word).glyph}
        glyphColor={wordStyle(row.word).color}
        title={row.title}
        right={<Meter done={row.done} total={row.total} color={wordStyle(row.word).color} />}
        second={row.progress ?? `${row.path} · ${row.age}`}
        onPress={() => navigation.navigate('Mission', { id: row.mission.id, title: row.title })}
      />}
      ListEmptyComponent={<Empty text={loadErrors.missions ? `Missions could not be loaded: ${loadErrors.missions}` : hasSynced ? 'No missions yet.' : 'Loading missions…'} />}
      ListFooterComponent={<View style={{ paddingBottom: 24 }}>
        {truncated.missions ? <Note tone="warning">More missions exist beyond these 200.</Note> : null}
        {hidden ? <Pressable onPress={() => setShowSystem(true)}><Note>{hidden} hidden (st's own, and failures before today) · tap to show</Note></Pressable> : null}
        <SectionHeader title="launches" count={open.length} />
        {open.map(launch => <ListRow key={launch.id} glyph="◇" glyphColor={theme.waiting} title={launch.title} right={<T dim>{launch.phase}</T>} second={`${launch.planner_config.provider} · ${launch.id}`} onPress={() => navigation.navigate('Launch', { id: launch.id })} />)}
        {truncated.launches ? <Note tone="warning">More launches exist; the CLI lists them all.</Note> : null}
        <Legend entries={legend} />
      </View>}
    />
  </Screen>;
}

// One mission: what finished, what is happening, what is next, and who holds each step.
export function MissionScreen({ route, navigation }: RootScreen<'Mission'>) {
  const { data, status, actions, caps } = useStore();
  const listed = data.missions.find(mission => mission.id === route.params.id);
  const [detail, setDetail] = useState<Mission | null>(null);
  useEffect(() => { let live = true; void actions.mission(route.params.id).then(found => { if (live && found) setDetail(found); }); return () => { live = false; }; }, [route.params.id, listed?.revision, status]); // eslint-disable-line react-hooks/exhaustive-deps
  const mission = detail ?? listed;
  useLayoutEffect(() => { navigation.setOptions({ title: route.params.title ?? (mission ? missionTitle(mission) : 'Mission') }); }, [navigation, route.params.title, mission]);
  if (!mission) return <Screen><Banners /><Empty text={status === 'online' ? 'Loading this mission…' : 'This mission is not in the last loaded data.'} /></Screen>;
  const word = missionWord(mission, data.attention, data.agents);
  const steps = [...missionSteps(mission)].sort((a, b) => stepStyle(a.state).rank - stepStyle(b.state).rank || a.path.localeCompare(b.path));
  const runs = mission.run_details ?? [];
  const outcome = runs.at(-1)?.outcome;
  // What this mission waits on the person for, first and answerable: each opens its card.
  const waiting = homeRows(data.attention.filter(item => item.mission_id === mission.id), caps?.session_actor);
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ paddingBottom: 32 }}>
      <View style={{ padding: 12, gap: 4 }}>
        <T><T bold color={wordStyle(word).color}>{wordStyle(word).glyph} {wordName(word)}</T><T dim>  {mission.state} · {mission.runs.length} run{mission.runs.length === 1 ? '' : 's'}</T></T>
        <T dim selectable>{mission.id}</T>
        {outcome ? <T soft>set {outcome.status}{outcome.previous_status ? ` (was ${outcome.previous_status})` : ''} by {outcome.actor}: {outcome.reason}</T> : null}
      </View>
      {waiting.length ? <>
        <SectionHeader title="needs you" count={waiting.length} color={theme.person} />
        {waiting.map(row => <ListRow
          key={row.item.id}
          glyph={row.glyph}
          glyphColor={theme[row.color]}
          title={<T numberOfLines={2}><T color={theme[row.color]}>{row.kind} </T><T bold>{row.title}</T></T>}
          second="tap to answer"
          onPress={() => navigation.navigate('Attention', { id: row.item.id })}
        />)}
      </> : null}
      <SectionHeader title="steps" count={steps.length} />
      {steps.map(step => {
        const style = stepStyle(step.state);
        const holder = step.agentless ? 'st' : step.claimant ?? step.assignee;
        const agent = holder ? data.agents.find(candidate => candidate.id === holder) : undefined;
        return <View key={step.id} style={{ paddingHorizontal: 12, paddingVertical: 5 }}>
          <T><T bold color={style.color}>{style.glyph} </T><T bold>{step.path}</T><T color={style.color}>  {style.word}</T></T>
          {step.blocked_reason ? <T color={theme.waiting} style={{ paddingLeft: 22 }}>{step.blocked_reason}</T> : null}
          {['claimed', 'working', 'running'].includes(step.state) && step.last_progress ? <T style={{ paddingLeft: 22 }}>{step.last_progress}</T> : null}
          {step.goals?.[0] ? <View style={{ paddingLeft: 22 }}><Markdown text={step.goals[0]} color={theme.subtext0} /></View> : null}
          {holder ? agent
            ? <Pressable onPress={() => navigation.navigate('Conversation', { target: agent.id, title: agentName(agent) })} style={{ paddingLeft: 22 }}><T><T dim>held by </T><T color={theme.accent}>{agentName(agent)}</T></T></Pressable>
            : <T dim style={{ paddingLeft: 22 }}>held by {holder}</T> : null}
        </View>;
      })}
      {!steps.length ? <Note>No steps are visible for this mission's runs.</Note> : null}
      {mission.visualization?.groups.filter(group => group.kind === 'nested-mission').map(group => <Note key={group.id}>nested mission: {group.members.join(', ')}</Note>)}
    </ScrollView>
  </Screen>;
}

const PLANNERS: Planner[] = ['codex', 'claude', 'pi', 'omp', 'opencode'];

// A launch: a planner turns the request into a proposed mission; nothing runs before approval.
export function NewMissionScreen({ navigation }: RootScreen<'NewMission'>) {
  const { busy, actions } = useStore();
  const [title, setTitle] = useState(''), [request, setRequest] = useState(''), [workspace, setWorkspace] = useState('');
  const [provider, setProvider] = useState<Planner>('codex'), [model, setModel] = useState(''), [effort, setEffort] = useState('');
  const ready = title.trim() && request.trim() && workspace.trim();
  useLayoutEffect(() => {
    navigation.setOptions({ unstable_headerLeftItems: () => [{ type: 'button', label: 'Cancel', onPress: () => navigation.goBack() }] });
  }, [navigation]);
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, paddingBottom: 48 }} keyboardShouldPersistTaps="handled" keyboardDismissMode="interactive">
      <T soft>Say what you want. A planner turns it into a proposed mission, which appears on Home for you to approve; nothing runs before that.</T>
      <Field placeholder="title" value={title} onChangeText={setTitle} />
      <Field placeholder="what should be done?" multiline value={request} onChangeText={setRequest} />
      <Field placeholder="workspace on the target machine" autoCapitalize="none" autoCorrect={false} spellCheck={false} value={workspace} onChangeText={setWorkspace} />
      <SectionHeader title="planner" />
      <SegmentedControl values={PLANNERS} selectedIndex={PLANNERS.indexOf(provider)} onChange={event => setProvider(PLANNERS[event.nativeEvent.selectedSegmentIndex])} appearance="dark" style={{ marginTop: 4 }} />
      <Field placeholder="model (optional)" autoCapitalize="none" autoCorrect={false} spellCheck={false} value={model} onChangeText={setModel} />
      <Field placeholder="effort (optional)" autoCapitalize="none" autoCorrect={false} spellCheck={false} value={effort} onChangeText={setEffort} />
      <Button label="create launch" disabled={busy || !ready} onPress={() => void actions.createLaunch({ title: title.trim(), request: request.trim(), workspace: workspace.trim(), provider, model: model.trim() || undefined, effort: effort.trim() || undefined }).then(done => { if (done) navigation.goBack(); })} />
    </ScrollView>
  </Screen>;
}

export function LaunchScreen({ route, navigation }: RootScreen<'Launch'>) {
  const { data, busy, status, actions } = useStore();
  const launch = data.launches.find(candidate => candidate.id === route.params.id);
  const [variants, setVariants] = useState<LaunchVariant[]>([]);
  const [feedback, setFeedback] = useState('');
  const reload = () => { if (launch) void actions.variants(launch.id).then(setVariants); };
  useEffect(reload, [launch?.id, launch?.revision, status]); // eslint-disable-line react-hooks/exhaustive-deps
  useLayoutEffect(() => { navigation.setOptions({ title: launch?.title ?? 'Launch' }); }, [navigation, launch?.title]);
  if (!launch) return <Screen><Banners /><Empty text="This launch is not in the last loaded list." /></Screen>;
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, paddingBottom: 48 }} keyboardShouldPersistTaps="handled">
      <T dim>{launch.phase} · {launch.planner_config.provider} · {launch.id}</T>
      <Markdown text={launch.request} color={theme.subtext0} />
      <SectionHeader title="variants" count={variants.length} />
      {variants.map(variant => <View key={variant.id} style={{ paddingVertical: 6 }}>
        <T bold>variant {variant.ordinal} · {variant.status}</T>
        <T dim>{variant.diagnostics.map(d => `${d.severity}: ${d.message}`).join(' · ') || 'no diagnostics'}</T>
        <View style={{ flexDirection: 'row', gap: 6 }}>
          <Button label="preview" disabled={busy} onPress={() => void actions.preview(launch, variant).then(reload)} />
          {variant.preview_token ? <Button label="approve" color={theme.person} disabled={busy} onPress={() => Alert.alert('Approve launch variant?', launch.title, [{ text: 'Cancel' }, { text: 'Approve', onPress: () => void actions.approve(launch, variant).then(done => { if (done) navigation.goBack(); }) }])} /> : null}
        </View>
      </View>)}
      {launch.phase === 'authoring' || launch.phase === 'review' ? <>
        <SectionHeader title="revise" />
        <Field placeholder="feedback for the planner" value={feedback} onChangeText={setFeedback} multiline />
        <Button label="revise" disabled={busy || !feedback.trim()} onPress={() => void actions.reviseLaunch(launch, feedback.trim()).then(done => { if (done) setFeedback(''); })} />
      </> : null}
    </ScrollView>
  </Screen>;
}

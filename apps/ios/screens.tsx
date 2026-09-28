// Every screen of the contract (docs/clients/ui-contract.md), the Expo way: list screens push
// detail screens, references open sheets, and the three list states stay apart.

import { useFocusEffect, useNavigation, useRoute } from '@react-navigation/native';
import { useCallback, useLayoutEffect, useState } from 'react';
import { FlatList, KeyboardAvoidingView, Pressable, ScrollView, SectionList, StyleSheet, Text, View } from 'react-native';
import { AttentionCard, agentName, Flow, missionName } from './AttentionCard';
import type { Agent, Attention, Mission, Step } from './clientView';
import { harnessName, items, progress } from './clientView';
import { ConversationView } from './ConversationView';
import { useLinks, type Nav } from './navigation';
import { agentPath, agentSections, agentsTree, hiddenSystemMissions, HOME_TEXT, homeSections, listState, missionHelp, missionPath, missionSections, missionsTree, worktreeId, worktreeSections, type TreeRow } from './screenModel';
import { useStore } from './store';
import { colors, type ColorToken } from './theme';
import { Body, Button, Buttons, Card, Composer, DemoBadge, Dim, Field, Glyph, Legend, ListStateView, Markdown, MONO, NoticeBar, Pill, Section, Segmented, styles as ui } from './ui';
import { agentInfo, attentionStyle, harnessColor, SPINNER, stepStyle, wordInfo } from './words';

type Params = { id?: string; name?: string; subject?: string; view?: string };
const useParams = () => (useRoute().params ?? {}) as Params;

/** The header buttons every tab shares: the demo label and settings. */
export function HeaderRight() {
  const navigation = useNavigation<Nav>();
  return (
    <View style={local.headerRight}>
      <DemoBadge />
      <Pressable accessibilityRole="button" accessibilityLabel="Settings" onPress={() => navigation.navigate('Settings')} hitSlop={10}>
        <Text style={local.gear}>⚙︎</Text>
      </Pressable>
    </View>
  );
}

function Row({ glyph, color, title, kind, right, second, onPress, depth = 0 }: { glyph: string; color: ColorToken; title: string; kind?: string; right?: React.ReactNode; second?: string; onPress: () => void; depth?: number }) {
  return (
    <Pressable onPress={onPress} style={({ pressed }) => [ui.row, { paddingLeft: 16 + depth * 16 }, pressed && { backgroundColor: colors.row_selected }]} accessibilityRole="button">
      <Glyph glyph={glyph} color={color} />
      <View style={ui.rowMain}>
        <Text style={ui.rowTitle} numberOfLines={kind ? 2 : 1}>{kind ? <Text style={{ color: colors[color], fontWeight: '500' }}>{`${kind}  `}</Text> : null}{title}</Text>
        {second ? <Dim lines={1}>{second}</Dim> : null}
      </View>
      {right ? <View style={ui.rowRight}>{right}</View> : null}
    </Pressable>
  );
}

function Folder({ row }: { row: Extract<TreeRow, { kind: 'folder' }> }) {
  return <Text style={[local.folder, { paddingLeft: 16 + row.depth * 16 }]}><Text style={{ color: colors.surface2 }}>▾ </Text>{row.name}</Text>;
}

function Screen({ children }: { children: React.ReactNode }) {
  return <View style={ui.screen}>{children}<NoticeBar /></View>;
}

// ---------------------------------------------------------------------- home

const HOME_LEGEND: [string, ColorToken, string][] = [['◆', 'person', 'decide'], ['✕', 'fault', 'fault'], ['✉', 'sapphire', 'message']];

export function HomeScreen() {
  const store = useStore();
  const { world, snoozed } = store;
  const navigation = useNavigation<Nav>();
  const state = listState(world.attention, HOME_TEXT.loading, HOME_TEXT.empty);
  const sections = homeSections(world, snoozed).map(section => ({ ...section, data: section.items }));
  const later = items(world.attention).filter(item => snoozed.has(item.id)).length;
  return (
    <Screen>
      <SectionList
        sections={sections}
        keyExtractor={item => item.id}
        contentInsetAdjustmentBehavior="automatic"
        stickySectionHeadersEnabled={false}
        renderSectionHeader={({ section }) => <Section title={section.title} count={section.count} color={section.key === 'stopped' ? 'person' : 'overlay1'} />}
        renderItem={({ item }) => {
          const { glyph, color } = attentionStyle(item.kind.kind);
          return (
            <Row glyph={glyph} color={color} kind={item.kind.kind} title={item.title} onPress={() => navigation.navigate('Attention', { id: item.id })}
              second={`${item.waiting ? `${item.waiting} · ` : ''}waited ${item.age}`} />
          );
        }}
        ListEmptyComponent={state.kind === 'empty' ? (
          <View style={local.nothing}>
            <Text style={local.nothingTitle}><Text style={{ color: colors.green }}>✓ </Text>Nothing needs you right now.</Text>
            <Dim>{`${world.quiet_missions} missions are running without you. They are on the Missions tab.`}</Dim>
          </View>
        ) : <ListStateView state={state} retry={() => void store.refresh()} />}
        ListFooterComponent={
          <View>
            {later ? <Dim style={local.note}>{`${later} put off until later · ${store.mode === 'demo' ? 'demo, ' : ''}this device only`}</Dim> : null}
            {world.attention.state === 'ready' && world.quiet_missions > 0 && sections.length ? <Dim style={local.note}>{`not shown: ${world.quiet_missions} missions that don't need you`}</Dim> : null}
            <Legend entries={HOME_LEGEND} />
          </View>
        }
        refreshing={false}
        onRefresh={() => void store.refresh()}
      />
    </Screen>
  );
}

export function AttentionScreen() {
  const { world } = useStore();
  const { id } = useParams();
  const navigation = useNavigation<Nav>();
  const item = items(world.attention).find(candidate => candidate.id === id);
  useLayoutEffect(() => { if (item) navigation.setOptions({ title: item.kind.kind }); }, [navigation, item?.kind.kind]);
  return (
    <Screen>
      <KeyboardAvoidingView behavior="padding" style={local.flex} keyboardVerticalOffset={90}>
        <ScrollView contentContainerStyle={ui.scroll} contentInsetAdjustmentBehavior="automatic" keyboardShouldPersistTaps="handled">
          {item ? <AttentionCard item={item} /> : world.attention.state === 'loading'
            ? <ListStateView state={{ kind: 'loading', text: HOME_TEXT.loading }} />
            : <Card><Body>This item is no longer open. Nothing more to do here.</Body><Buttons><Button label="Back to Home" onPress={() => navigation.goBack()} /></Buttons></Card>}
        </ScrollView>
      </KeyboardAvoidingView>
    </Screen>
  );
}

// --------------------------------------------------------------------- agents

const AGENT_LEGEND: [string, ColorToken, string][] = [['◆', 'person', 'needs you'], [SPINNER[0], 'working', 'working'], ['●', 'idle', 'idle'], ['✕', 'fault', 'broken'], ['○', 'quiet', 'stopped'], ['?', 'quiet', 'unmanaged']];

function AgentRight({ agent }: { agent: Agent }) {
  return <><Text style={{ color: colors[harnessColor(agent.harness)], fontSize: 13 }}>{harnessName(agent.harness)}</Text><Dim>{agent.activity}</Dim></>;
}

export function AgentsScreen() {
  const store = useStore();
  const { world } = store;
  const navigation = useNavigation<Nav>();
  const [view, setView] = useState<'groups' | 'tree'>(useParams().view === 'tree' ? 'tree' : 'groups');
  const state = listState(world.agents, 'Loading agents…', 'No agents yet.');
  const open = (agent: Agent) => navigation.navigate('Agent', { id: agent.id });
  const agentsById = new Map(items(world.agents).map(agent => [agent.id, agent]));
  const header = <Segmented options={[['groups', 'Groups'], ['tree', 'Tree']]} value={view} onChange={setView} />;
  const footer = <Legend entries={AGENT_LEGEND} />;
  const empty = <ListStateView state={state} retry={() => void store.refresh()} />;
  return (
    <Screen>
      {view === 'groups' ? (
        <SectionList
          sections={agentSections(world).map(section => ({ ...section, data: section.items }))}
          keyExtractor={agent => agent.id}
          contentInsetAdjustmentBehavior="automatic"
          stickySectionHeadersEnabled={false}
          ListHeaderComponent={header} ListFooterComponent={footer} ListEmptyComponent={empty}
          renderSectionHeader={({ section }) => <Section title={section.title} count={section.count} color={section.key === 'waiting on you' ? 'person' : 'overlay1'} />}
          renderItem={({ item: agent }) => {
            const info = agentInfo(agent.state);
            return <Row glyph={info.glyph} color={info.color} title={agent.name} second={agentPath(agent)} right={<AgentRight agent={agent} />} onPress={() => open(agent)} />;
          }}
          refreshing={false} onRefresh={() => void store.refresh()}
        />
      ) : (
        <FlatList
          data={agentsTree(world)}
          keyExtractor={row => (row.kind === 'folder' ? `folder:${row.key}` : row.id)}
          contentInsetAdjustmentBehavior="automatic"
          ListHeaderComponent={header} ListFooterComponent={footer} ListEmptyComponent={empty}
          renderItem={({ item: row }) => {
            if (row.kind === 'folder') return <Folder row={row} />;
            const agent = agentsById.get(row.id)!;
            const info = agentInfo(agent.state);
            return <Row depth={row.depth} glyph={info.glyph} color={info.color} title={agent.name} right={<AgentRight agent={agent} />} onPress={() => open(agent)} />;
          }}
        />
      )}
    </Screen>
  );
}

export function AgentScreen() {
  const store = useStore();
  const { world } = store;
  const { id = '' } = useParams();
  const navigation = useNavigation<Nav>();
  const { peek } = useLinks();
  const [draft, setDraft] = useState('');
  const agent = items(world.agents).find(candidate => candidate.id === id);
  useFocusEffect(useCallback(() => store.watchConversation(id), [store.watchConversation, id]));
  useLayoutEffect(() => {
    navigation.setOptions({
      title: agent?.name ?? agentName(world, id),
      headerRight: () => agent && !agent.unmanaged ? <Pressable onPress={() => navigation.navigate('AgentDetails', { id })} hitSlop={10} accessibilityRole="button"><Text style={local.headerButton}>Details</Text></Pressable> : null,
    });
  }, [navigation, agent?.name, agent?.unmanaged, id]);
  if (!agent) {
    return <Screen><ListStateView state={world.agents.state === 'loading' ? { kind: 'loading', text: 'Loading agents…' } : { kind: 'empty', text: `${id} is not in the current view.` }} /></Screen>;
  }
  const info = agentInfo(agent.state);
  const elsewhere = !agent.unmanaged && agent.host !== '?' && agent.host !== world.host;
  const header = (
    <View style={local.agentHeader}>
      <View style={local.line}>
        <Glyph glyph={info.glyph} color={info.color} />
        <Text style={[local.agentState, { color: colors[info.color] }]}>{info.name}</Text>
        <Text style={{ color: colors[harnessColor(agent.harness)] }}>{harnessName(agent.harness)}</Text>
        <Dim>· {agent.host}</Dim>
      </View>
      <Dim>{agent.id}{agent.worktree ? `  ·  ${agent.worktree}` : ''}</Dim>
      {agent.mission && agent.step ? (
        <Text style={local.linkLine}><Text style={local.linkPrefix}>mission </Text><Text style={local.link} onPress={() => peek(agent.mission!)}>{`${missionName(world, agent.mission)} › ${agent.step}`}</Text></Text>
      ) : null}
      {elsewhere ? <Dim>{`${agent.name} runs on ${agent.host}. From ${world.host}, st may show only its Small Talk mail, not its transcript.`}</Dim> : null}
    </View>
  );
  const send = async () => {
    const text = draft.trim();
    if (!text) return;
    setDraft('');
    if (!(await store.send(agent.id, text))) setDraft(text);
  };
  return (
    <Screen>
      <KeyboardAvoidingView behavior="padding" style={local.flex} keyboardVerticalOffset={90}>
        <ConversationView load={world.conversations[agent.id]} header={header}
          empty={agent.unmanaged ? 'st did not start this process, so there is no conversation to show.' : 'Nothing said yet.'} />
        {agent.unmanaged
          ? <Dim style={local.unmanaged}>Found running, not started by st: messages cannot reach it. Start it through st to talk to it here.</Dim>
          : <Composer value={draft} onChange={setDraft} placeholder={`Message ${agent.name}`} onSend={() => void send()} />}
      </KeyboardAvoidingView>
    </Screen>
  );
}

/** What the agent holds now, what is queued next, and how it runs. */
export function AgentDetailsScreen() {
  const { world } = useStore();
  const { id } = useParams();
  const { peek } = useLinks();
  const agent = items(world.agents).find(candidate => candidate.id === id);
  if (!agent) return <View style={local.sheet}><Body color="subtext0">{`${id} is not in the current view.`}</Body></View>;
  const { details } = agent;
  const info = agentInfo(agent.state);
  const unknown = 'st has not said';
  // A form sheet sizes to its content, so the scroll view is its direct child with no flex.
  return (
    <ScrollView style={local.sheetScroll} contentContainerStyle={ui.scroll}>
        <Text style={local.title}>{agent.name}</Text>
        {details.fault ? <Card title="broken" color="fault" heavy><Markdown text={details.fault} /></Card> : null}
        <Card title="now">
          {agent.mission && agent.step ? (
            <>
              <Text style={local.link} onPress={() => peek(agent.mission!)}>{`${missionName(world, agent.mission)} › ${agent.step}`}</Text>
              {details.goal ? <Markdown text={details.goal} color="subtext0" /> : null}
              {details.claimed ? <Dim>{`held since ${details.claimed}`}</Dim> : null}
            </>
          ) : <Dim>No step right now.</Dim>}
        </Card>
        <Card title={`next · ${details.queued}`}>
          {details.next ? <Body><Text style={{ color: colors.accent }}>› </Text>{details.next}</Body> : <Dim>Nothing queued.</Dim>}
          {details.queue.filter(item => item !== details.next).map(item => <Body key={item} color="subtext0">· {item}</Body>)}
        </Card>
        <Card title="runs as">
          <View style={local.line}><Glyph glyph={info.glyph} color={info.color} /><Text style={{ color: colors[info.color] }}>{info.name}</Text></View>
          <Field label="harness" color={harnessColor(agent.harness)}>{harnessName(agent.harness)}</Field>
          <Field label="state" color={details.harness_state ? 'subtext0' : 'overlay0'}>{details.harness_state ?? unknown}</Field>
          <Field label="runtime" color={details.runtime ? 'subtext0' : 'overlay0'}>{details.runtime ?? unknown}</Field>
          <Field label="host">{agent.host}</Field>
          <Field label="worktree" color={agent.worktree ? 'subtext0' : 'overlay0'}>{agent.worktree ?? unknown}</Field>
          {details.under ? <Field label="under">{details.under}</Field> : null}
        </Card>
        <Dim>{agent.id}</Dim>
    </ScrollView>
  );
}

// ------------------------------------------------------------------- missions

const MISSION_LEGEND: [string, ColorToken, string][] = [['◆', 'person', 'needs you'], ['▲', 'fault', 'stalled'], ['◇', 'waiting', 'unstaffed'], ['◌', 'overlay1', 'queued'], ['◉', 'sapphire', 'watching'], [SPINNER[0], 'working', 'working'], ['●', 'idle', 'idle'], ['✓', 'done', 'done']];

function MissionRight({ mission }: { mission: Mission }) {
  const [done, total] = progress(mission);
  // st reports only current steps for many runs, so a 0/1 meter would claim too much.
  return <Dim>{done > 0 ? `${done}/${total}` : `${total} step${total === 1 ? '' : 's'}`}</Dim>;
}

export function MissionsScreen() {
  const store = useStore();
  const { world } = store;
  const navigation = useNavigation<Nav>();
  const [view, setView] = useState<'groups' | 'tree'>(useParams().view === 'tree' ? 'tree' : 'groups');
  const [system, setSystem] = useState(false);
  const state = listState(world.missions, 'Loading missions…', 'No missions yet.');
  const hidden = hiddenSystemMissions(world, system);
  const byId = new Map(items(world.missions).map(mission => [mission.id, mission]));
  const open = (mission: Mission) => navigation.navigate('Mission', { id: mission.id });
  const header = <Segmented options={[['groups', 'Groups'], ['tree', 'Tree']]} value={view} onChange={setView} />;
  const footer = (
    <View>
      {hidden || system ? <Pressable onPress={() => setSystem(!system)}><Dim style={local.note}>{system ? 'Hide system missions' : `${hidden} system missions hidden · show them`}</Dim></Pressable> : null}
      <Legend entries={MISSION_LEGEND} />
    </View>
  );
  const empty = <ListStateView state={state} retry={() => void store.refresh()} />;
  return (
    <Screen>
      {view === 'groups' ? (
        <SectionList
          sections={missionSections(world, system).map(section => ({ ...section, data: section.items }))}
          keyExtractor={mission => mission.id}
          contentInsetAdjustmentBehavior="automatic"
          stickySectionHeadersEnabled={false}
          ListHeaderComponent={header} ListFooterComponent={footer} ListEmptyComponent={empty}
          renderSectionHeader={({ section }) => <Section title={section.title} count={section.count} color={wordInfo(section.data[0].word).color} />}
          renderItem={({ item: mission }) => {
            const info = wordInfo(mission.word);
            return <Row glyph={info.glyph} color={info.color} title={mission.title} second={`${missionPath(mission)} · ${mission.age}`} right={<MissionRight mission={mission} />} onPress={() => open(mission)} />;
          }}
          refreshing={false} onRefresh={() => void store.refresh()}
        />
      ) : (
        <FlatList
          data={missionsTree(world, system)}
          keyExtractor={row => (row.kind === 'folder' ? `folder:${row.key}` : row.id)}
          contentInsetAdjustmentBehavior="automatic"
          ListHeaderComponent={header} ListFooterComponent={footer} ListEmptyComponent={empty}
          renderItem={({ item: row }) => {
            if (row.kind === 'folder') return <Folder row={row} />;
            const mission = byId.get(row.id)!;
            const info = wordInfo(mission.word);
            const [done, total] = progress(mission);
            return <Row depth={row.depth} glyph={info.glyph} color={info.color} title={row.name} right={<Text style={{ color: colors[info.color], fontSize: 13 }}>{`${info.name} ${done}/${total}`}</Text>} onPress={() => open(mission)} />;
          }}
        />
      )}
    </Screen>
  );
}

function StepRow({ step, open, toggle }: { step: Step; open: boolean; toggle: () => void }) {
  const style = stepStyle(step.state);
  const lines: [string, string[], ColorToken][] = [
    ['goal', step.goals, 'text'], ['constraint', step.constraints, 'subtext0'], ['gate', step.gates, 'person'], ['blocked by', step.blockers, 'red'],
    ...(step.after.length ? [['after', [step.after.join(', ')], 'subtext0'] as [string, string[], ColorToken]] : []),
    ['attempt', [String(step.attempt)], 'subtext0'],
  ];
  return (
    <View>
      <Pressable onPress={toggle} accessibilityRole="button" accessibilityState={{ expanded: open }} style={local.stepRow}>
        <Dim>{open ? '▾' : '▸'}</Dim>
        <Glyph glyph={style.glyph} color={style.color} size={13} />
        <Text style={local.stepName} numberOfLines={1}>{step.name}</Text>
        <Text style={[local.stepWord, { color: colors[style.color] }]}>{style.word}</Text>
        <Text style={[local.stepOwner, { color: step.owner ? colors.subtext0 : colors.waiting }]} numberOfLines={1}>{step.owner ?? 'nobody'}</Text>
        <Dim>{step.age}</Dim>
      </Pressable>
      {step.note ? <Dim style={local.stepNote}>{step.note}</Dim> : null}
      {open ? (
        <View style={local.stepMore}>
          {lines.flatMap(([label, values, color]) => values.map((value, index) => (
            <Field key={`${label}${index}`} label={index === 0 ? label : ''} color={color}>{value}</Field>
          )))}
        </View>
      ) : null}
    </View>
  );
}

function MissionHelp({ mission }: { mission: Mission }) {
  const store = useStore();
  const { peek } = useLinks();
  const help = missionHelp(store.world, mission);
  if (help.kind === 'none') return null;
  if (help.kind === 'queued') {
    return <Card title="nothing for you to do"><Markdown text={help.note} /><Dim>It starts by itself when the agent is free.</Dim></Card>;
  }
  const color: ColorToken = help.word === 'unstaffed' ? 'waiting' : 'fault';
  const style = help.step ? stepStyle(help.step.state) : null;
  return (
    <Card title="what you can do" color={color} heavy>
      {help.step && style ? (
        <Text style={local.stuck}>
          <Text style={{ color: colors[style.color], fontWeight: '700' }}>{style.glyph} {help.step.name} </Text>
          <Text style={{ color: colors[style.color] }}>{help.step.state === 'failed' ? 'failed' : `is ${style.word}`}</Text>
          {help.step.attempt > 1 ? <Text style={{ color: colors.overlay0 }}>{` after ${help.step.attempt} attempts`}</Text> : null}
        </Text>
      ) : null}
      {help.step?.blockers.map(blocker => <Body key={blocker} color="subtext0">because {blocker}</Body>)}
      {help.broken ? <Body>{`${help.broken.name} is ${agentInfo(help.broken.state).name}. Fix the agent and the step can run again.`}</Body> : null}
      <Buttons>
        {help.broken ? <Button label="Restart the agent" color="green" onPress={() => void store.missionAction(mission, 'restart')} /> : null}
        {help.broken ? <Button label="Chat with it" color="sapphire" onPress={() => peek(help.broken!.id)} /> : null}
        <Button label="Retry the step" color="yellow" onPress={() => void store.missionAction(mission, 'retry')} />
        <Button label="Cancel this run" color="red" confirm="Cancel this run and stop its work" onPress={() => void store.missionAction(mission, 'cancel')} />
      </Buttons>
    </Card>
  );
}

function Meter({ done, total, color }: { done: number; total: number; color: ColorToken }) {
  return <View style={local.meter}><View style={[local.meterFill, { flex: done, backgroundColor: colors[color] }]} /><View style={{ flex: Math.max(0, total - done) }} /></View>;
}

export function MissionScreen() {
  const { world } = useStore();
  const { id } = useParams();
  const navigation = useNavigation<Nav>();
  const { peek } = useLinks();
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  const mission = items(world.missions).find(candidate => candidate.id === id);
  useLayoutEffect(() => { navigation.setOptions({ title: mission?.title ?? 'Mission' }); }, [navigation, mission?.title]);
  if (!mission) {
    return <Screen><ListStateView state={world.missions.state === 'loading' ? { kind: 'loading', text: 'Loading missions…' } : { kind: 'empty', text: `${id} is not in the current view.` }} /></Screen>;
  }
  const info = wordInfo(mission.word);
  const decision = mission.decision ? items(world.attention).find(item => item.id === mission.decision) : undefined;
  const [done, total] = progress(mission);
  const toggle = (name: string) => setOpen(current => { const next = new Set(current); if (next.has(name)) next.delete(name); else next.add(name); return next; });
  const agents = mission.agents.map(agentId => items(world.agents).find(agent => agent.id === agentId)).filter((agent): agent is Agent => agent !== undefined);
  return (
    <Screen>
      <KeyboardAvoidingView behavior="padding" style={local.flex} keyboardVerticalOffset={90}>
        <ScrollView contentContainerStyle={ui.scroll} contentInsetAdjustmentBehavior="automatic" keyboardShouldPersistTaps="handled">
          <View style={local.missionHead}>
            <Pill text={`${info.glyph} ${info.name}`} color={info.color} />
            <Text style={local.title}>{mission.title}</Text>
            <Dim>{mission.host ? `on ${mission.host} · ${mission.age}` : mission.age}</Dim>
            <Dim>{mission.id}</Dim>
            <Text style={{ color: colors[info.color], fontSize: 14 }}>{info.explain}</Text>
          </View>
          {mission.goals.length ? (
            <Card title="goals" color="lavender">
              {mission.goals.map(goal => <View key={goal} style={local.goal}><Text style={{ color: colors.lavender }}>◆</Text><Markdown text={goal} style={local.flex} /></View>)}
            </Card>
          ) : null}
          {/* The same card Home shows, so the answer can be given right here. */}
          {decision ? <AttentionCard item={decision} /> : null}
          <MissionHelp mission={mission} />
          <Card title={`steps · ${done}/${total} done`}>
            <Meter done={done} total={total} color="done" />
            <Flow steps={mission.steps.map(step => ({ name: step.name, after: step.after, color: stepStyle(step.state).color }))} />
            {mission.steps.map(step => <StepRow key={step.name} step={step} open={open.has(step.name)} toggle={() => toggle(step.name)} />)}
            {mission.steps.length ? <Dim>tap a step to open it</Dim> : <Dim>st reports no steps for this run.</Dim>}
          </Card>
          <Card title={`agents · ${mission.agents.length}`}>
            {agents.length ? agents.map(agent => {
              const agentState = agentInfo(agent.state);
              return (
                <Pressable key={agent.id} onPress={() => peek(agent.id)} style={local.line}>
                  <Glyph glyph={agentState.glyph} color={agentState.color} />
                  <Text style={[local.agentName, { color: colors[agentState.color] }]}>{agent.name}</Text>
                  <Text style={{ color: colors[agentState.color] }}>{agentState.name}</Text>
                  <Text style={{ color: colors[harnessColor(agent.harness)] }}>{harnessName(agent.harness)}</Text>
                  <Dim>{`on ${agent.host} · ${agent.activity}`}</Dim>
                </Pressable>
              );
            }) : <Dim>No agents right now.</Dim>}
          </Card>
          {mission.worktree ? <Card title="worktree"><Body>{mission.worktree}<Text style={{ color: colors.overlay0 }}>{`  on ${mission.host}`}</Text></Body></Card> : null}
          <Buttons><Button label="The whole declaration" color="overlay1" onPress={() => navigation.navigate('Declaration', { id: mission.id })} /></Buttons>
        </ScrollView>
      </KeyboardAvoidingView>
    </Screen>
  );
}

/** The mission as written, when st provides it. */
export function DeclarationScreen() {
  const { world } = useStore();
  const { id } = useParams();
  const mission = items(world.missions).find(candidate => candidate.id === id);
  const color = (line: string) => {
    const trimmed = line.trimStart();
    if (trimmed.startsWith('//')) return colors.overlay0;
    if (trimmed.startsWith('goal') || trimmed.startsWith('constraint')) return colors.lavender;
    if (trimmed.startsWith('gate') || trimmed.startsWith('reviewer') || trimmed.startsWith('question')) return colors.person;
    if (trimmed.startsWith('step') || trimmed.startsWith('mission')) return colors.peach;
    return colors.subtext1;
  };
  return (
    <Screen>
      <ScrollView contentContainerStyle={ui.scroll} contentInsetAdjustmentBehavior="automatic">
        <Dim>{id}</Dim>
        {mission?.kdl ? (
          <ScrollView horizontal>
            <Text selectable style={local.kdl}>{mission.kdl.split('\n').map((line, index) => <Text key={index} style={{ color: color(line) }}>{line}{'\n'}</Text>)}</Text>
          </ScrollView>
        ) : <Body color="subtext0">st does not send mission declarations to clients yet. Until it does, read it with: st missions show</Body>}
      </ScrollView>
    </Screen>
  );
}

// ---------------------------------------------------------------------- fleet

export function FleetScreen() {
  const store = useStore();
  const { world } = store;
  const navigation = useNavigation<Nav>();
  const state = listState(world.machines, 'Loading machines…', 'No machines yet.');
  return (
    <Screen>
      <FlatList
        data={items(world.machines)}
        keyExtractor={machine => machine.name}
        contentInsetAdjustmentBehavior="automatic"
        ListEmptyComponent={<ListStateView state={state} retry={() => void store.refresh()} />}
        ListFooterComponent={<Legend entries={[['●', 'green', 'online'], ['○', 'red', 'offline']]} />}
        renderItem={({ item: machine }) => {
          const agents = items(world.agents).filter(agent => agent.host === machine.name).length;
          return (
            <Row glyph={machine.online ? '●' : '○'} color={machine.online ? 'green' : 'red'} title={machine.you_are_here ? `${machine.name}  · you are here` : machine.name}
              second={`${machine.platform} · seen ${machine.seen}`} right={<Dim>{`${agents} agent${agents === 1 ? '' : 's'}`}</Dim>} onPress={() => navigation.navigate('Machine', { name: machine.name })} />
          );
        }}
        refreshing={false} onRefresh={() => void store.refresh()}
      />
    </Screen>
  );
}

function AgentLine({ agent, extra }: { agent: Agent; extra?: string }) {
  const { peek } = useLinks();
  const info = agentInfo(agent.state);
  return (
    <Pressable onPress={() => peek(agent.id)} style={local.line} accessibilityRole="button">
      <Glyph glyph={info.glyph} color={info.color} />
      <Text style={[local.agentName, { color: colors[info.color] }]}>{agent.name}</Text>
      <Text style={{ color: colors[harnessColor(agent.harness)] }}>{harnessName(agent.harness)}</Text>
      {extra ? <Dim lines={1}>{extra}</Dim> : null}
    </Pressable>
  );
}

export function MachineScreen() {
  const { world } = useStore();
  const { name } = useParams();
  const navigation = useNavigation<Nav>();
  const machine = items(world.machines).find(candidate => candidate.name === name);
  useLayoutEffect(() => { navigation.setOptions({ title: name }); }, [navigation, name]);
  if (!machine) return <Screen><ListStateView state={{ kind: 'empty', text: `${name} is not in the current view.` }} /></Screen>;
  const agents = items(world.agents).filter(agent => agent.host === machine.name);
  return (
    <Screen>
      <ScrollView contentContainerStyle={ui.scroll} contentInsetAdjustmentBehavior="automatic">
        <View style={local.line}>
          <Glyph glyph={machine.online ? '●' : '○'} color={machine.online ? 'green' : 'red'} />
          <Text style={local.title}>{machine.name}</Text>
          {machine.you_are_here ? <Text style={{ color: colors.accent }}>you are here</Text> : null}
        </View>
        <Dim>{`${machine.platform} · seen ${machine.seen}`}</Dim>
        {machine.load ? <Body color="subtext0">{machine.load}</Body> : null}
        <Card title="reaches">
          {machine.links.length ? machine.links.map(([peer, up, detail]) => (
            <View key={peer} style={local.line}><Glyph glyph={up ? '●' : '○'} color={up ? 'green' : 'red'} size={12} /><Text style={local.agentName}>{peer}</Text><Dim>{detail}</Dim></View>
          )) : <Dim>Unknown while this machine is offline.</Dim>}
        </Card>
        <Card title="agents here">
          {agents.length ? agents.map(agent => <AgentLine key={agent.id} agent={agent} extra={agent.worktree ?? undefined} />) : <Dim>No agents on this machine.</Dim>}
        </Card>
      </ScrollView>
    </Screen>
  );
}

// ------------------------------------------------------------------ worktrees

function DemoBanner() {
  return (
    <View style={local.banner}>
      <View style={local.bannerPill}><Text style={local.bannerPillText}>DEMO DATA</Text></View>
      <Text style={local.bannerText}>st does not track worktrees yet. Everything on this tab is invented.</Text>
    </View>
  );
}

export function WorktreesScreen() {
  const { world } = useStore();
  const navigation = useNavigation<Nav>();
  const state = listState(world.worktrees, 'Loading worktrees…', 'No worktrees known.');
  return (
    <Screen>
      <SectionList
        sections={worktreeSections(world).map(section => ({ ...section, data: section.items }))}
        keyExtractor={worktreeId}
        contentInsetAdjustmentBehavior="automatic"
        stickySectionHeadersEnabled={false}
        ListHeaderComponent={<DemoBanner />}
        ListEmptyComponent={<ListStateView state={state} />}
        ListFooterComponent={<Legend entries={[['↑', 'green', 'ahead'], ['↓', 'red', 'behind'], ['±', 'yellow', 'changed']]} />}
        renderSectionHeader={({ section }) => <Section title={section.title} count={section.count} />}
        renderItem={({ item: tree }) => (
          <Pressable onPress={() => navigation.navigate('Worktree', { id: worktreeId(tree) })} style={({ pressed }) => [ui.row, pressed && { backgroundColor: colors.row_selected }]}>
            <View style={ui.rowMain}>
              <Text style={[ui.rowTitle, { color: colors.lavender }]}>{tree.branch}</Text>
              <Dim lines={1}>{`${tree.path} · ${tree.agents.length} agent${tree.agents.length === 1 ? '' : 's'}`}</Dim>
            </View>
            <Text style={local.counts}>
              {tree.ahead ? <Text style={{ color: colors.green }}>{`↑${tree.ahead} `}</Text> : null}
              {tree.behind ? <Text style={{ color: colors.red }}>{`↓${tree.behind} `}</Text> : null}
              {tree.dirty ? <Text style={{ color: colors.yellow }}>{`±${tree.dirty}`}</Text> : null}
            </Text>
          </Pressable>
        )}
      />
    </Screen>
  );
}

export function WorktreeScreen() {
  const { world } = useStore();
  const { id } = useParams();
  const { peek } = useLinks();
  const tree = items(world.worktrees).find(candidate => worktreeId(candidate) === id);
  if (!tree) return <Screen><DemoBanner /><ListStateView state={{ kind: 'empty', text: 'This worktree is not in the current view.' }} /></Screen>;
  const agents = tree.agents.map(agentId => items(world.agents).find(agent => agent.id === agentId)).filter((agent): agent is Agent => agent !== undefined);
  const missions = tree.missions.map(missionId => items(world.missions).find(mission => mission.id === missionId)).filter((mission): mission is Mission => mission !== undefined);
  return (
    <Screen>
      <ScrollView contentContainerStyle={ui.scroll} contentInsetAdjustmentBehavior="automatic">
        <DemoBanner />
        <Text style={[local.title, { color: colors.lavender }]}>{tree.branch}</Text>
        <Dim>{`${tree.path} on ${tree.host}`}</Dim>
        <Body color="subtext0">{`${tree.ahead} ahead · ${tree.behind} behind · ${tree.dirty} changed files`}</Body>
        <Card title="agents here">
          {agents.length ? agents.map(agent => <AgentLine key={agent.id} agent={agent} extra={agentInfo(agent.state).name} />) : <Dim>Nobody is working here.</Dim>}
        </Card>
        <Card title="missions here">
          {missions.length ? missions.map(mission => {
            const info = wordInfo(mission.word);
            return (
              <Pressable key={mission.id} onPress={() => peek(mission.id)} style={local.line}>
                <Glyph glyph={info.glyph} color={info.color} />
                <Text style={[local.agentName, { color: colors[info.color] }]}>{mission.title}</Text>
                <Text style={{ color: colors[info.color] }}>{info.name}</Text>
              </Pressable>
            );
          }) : <Dim>No missions use this worktree.</Dim>}
        </Card>
      </ScrollView>
    </Screen>
  );
}

// ---------------------------------------------------------------------- peek

/** The sheet a tapped reference opens: enough to recognise it, Go to, and Message. */
export function PeekScreen() {
  const { world } = useStore();
  const { subject = '' } = useParams();
  const { goTo, navigation } = useLinks();
  const leaveAnd = (target: string) => { navigation.goBack(); goTo(target); };
  const agent = items(world.agents).find(candidate => candidate.id === subject);
  const mission = items(world.missions).find(candidate => candidate.id === subject);
  const item: Attention | undefined = items(world.attention).find(candidate => candidate.id === subject);
  let body: React.ReactNode;
  if (agent) {
    const info = agentInfo(agent.state);
    body = (
      <>
        <View style={local.line}><Glyph glyph={info.glyph} color={info.color} /><Text style={local.title}>{agent.name}</Text><Text style={{ color: colors[info.color] }}>{info.name}</Text></View>
        <Dim>{agent.id}</Dim>
        <Field label="harness" color={harnessColor(agent.harness)}>{harnessName(agent.harness)}</Field>
        <Field label="host">{agent.host}</Field>
        {agent.worktree ? <Field label="worktree">{agent.worktree}</Field> : null}
        {agent.mission && agent.step ? <Field label="doing" color="text">{`${missionName(world, agent.mission)} › ${agent.step}`}</Field> : null}
        <Field label="last seen">{`${agent.activity} ago`}</Field>
        <Buttons>
          <Button label="Go to agent" filled onPress={() => leaveAnd(agent.id)} />
          {agent.unmanaged ? null : <Button label="Message" color="sapphire" onPress={() => leaveAnd(agent.id)} />}
        </Buttons>
      </>
    );
  } else if (mission) {
    const info = wordInfo(mission.word);
    const [done, total] = progress(mission);
    const names = mission.agents.map(id => items(world.agents).find(candidate => candidate.id === id)?.name).filter(Boolean);
    body = (
      <>
        <View style={local.line}><Glyph glyph={info.glyph} color={info.color} /><Text style={local.title}>{mission.title}</Text></View>
        <Text style={{ color: colors[info.color] }}>{`${info.name} · ${info.explain}`}</Text>
        <Dim>{mission.id}</Dim>
        <Field label="steps">{`${done} of ${total} done`}</Field>
        {mission.steps.filter(step => step.state !== 'done' && step.state !== 'pending').slice(0, 3).map(step => {
          const style = stepStyle(step.state);
          return <Text key={step.name} style={local.peekStep}><Text style={{ color: colors[style.color] }}>{style.glyph} </Text><Text style={{ color: colors.text }}>{step.name} </Text><Text style={{ color: colors[style.color] }}>{style.word}</Text><Text style={{ color: colors.overlay0 }}>{`  ${step.owner ?? 'nobody'}`}</Text></Text>;
        })}
        {names.length ? <Field label="agents">{names.join(', ')}</Field> : null}
        <Buttons><Button label="Go to mission" filled onPress={() => leaveAnd(mission.id)} /></Buttons>
      </>
    );
  } else if (item) {
    const { glyph, color } = attentionStyle(item.kind.kind);
    body = (
      <>
        <Text style={local.title}><Text style={{ color: colors[color] }}>{`${glyph} ${item.kind.kind} `}</Text>{item.title}</Text>
        <Dim>{`waiting ${item.age}`}</Dim>
        <Buttons><Button label="Open on Home" color="person" filled onPress={() => leaveAnd(item.id)} /></Buttons>
      </>
    );
  } else body = <Body color="subtext0">{`${subject} is not in the current view.`}</Body>;
  return <View style={local.sheet}>{body}</View>;
}

// ------------------------------------------------------------------ settings

export function SettingsScreen() {
  const store = useStore();
  const { connection } = store;
  return (
    <Screen>
      <ScrollView contentContainerStyle={ui.scroll}>
        <Card title="connection">
          <Field label="mode" color="text">{store.mode === 'demo' ? 'demo' : 'live'}</Field>
          <Field label="status">{connection.status}</Field>
          <Field label="person">{connection.person || 'st has not said yet'}</Field>
          {connection.gateway ? <Field label="gateway">{connection.gateway}</Field> : null}
          <Field label="host">{store.world.host}</Field>
        </Card>
        <Buttons>
          {store.mode === 'live' ? <Button label="Refresh now" onPress={() => void store.refresh()} /> : null}
          <Button label={store.mode === 'demo' ? 'Leave the demo' : 'Forget this device'} color="red" confirm={store.mode === 'demo' ? 'Leave the demo' : 'Forget the paired credential'} onPress={() => void store.leave()} />
        </Buttons>
        <Dim>{'The Home badge counts everything open; it turns mauve when somebody is stopped on you. Mauve means a person is needed and is used for nothing else.'}</Dim>
      </ScrollView>
    </Screen>
  );
}

const local = StyleSheet.create({
  flex: { flex: 1 },
  headerRight: { flexDirection: 'row', alignItems: 'center', gap: 12 },
  gear: { color: colors.subtext0, fontSize: 20 },
  headerButton: { color: colors.accent, fontSize: 16, fontWeight: '600' },
  folder: { color: colors.overlay1, paddingVertical: 6, fontSize: 14 },
  nothing: { padding: 24, gap: 8 },
  nothingTitle: { color: colors.text, fontSize: 17, fontWeight: '700' },
  note: { paddingHorizontal: 16, paddingTop: 10 },
  line: { flexDirection: 'row', alignItems: 'center', gap: 8, flexWrap: 'wrap' },
  title: { color: colors.text, fontSize: 20, fontWeight: '700' },
  agentHeader: { gap: 4, paddingBottom: 12, marginBottom: 12, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: colors.surface1 },
  agentState: { fontWeight: '700', flex: 1 },
  agentName: { color: colors.text, fontSize: 14, fontWeight: '600', minWidth: 110 },
  linkLine: { fontSize: 14 },
  linkPrefix: { color: colors.overlay0 },
  link: { color: colors.blue, textDecorationLine: 'underline', fontSize: 14 },
  unmanaged: { padding: 12, backgroundColor: colors.mantle },
  missionHead: { gap: 4 },
  goal: { flexDirection: 'row', gap: 6 },
  meter: { flexDirection: 'row', height: 6, borderRadius: 3, backgroundColor: colors.surface0, overflow: 'hidden' },
  meterFill: { borderRadius: 3 },
  stepRow: { flexDirection: 'row', alignItems: 'center', gap: 6, paddingVertical: 6 },
  stepName: { color: colors.text, fontSize: 14, flex: 1 },
  stepWord: { fontSize: 13, width: 70 },
  stepOwner: { fontSize: 13, width: 100 },
  stepNote: { paddingLeft: 30, fontSize: 12 },
  stepMore: { paddingLeft: 30, paddingBottom: 8, gap: 2 },
  stuck: { fontSize: 15 },
  kdl: { fontFamily: MONO, fontSize: 12, lineHeight: 17 },
  counts: { fontSize: 13, fontWeight: '700' },
  banner: { margin: 16, marginBottom: 4, gap: 6, padding: 12, borderRadius: 10, backgroundColor: colors.mantle, borderWidth: 1, borderColor: colors.yellow },
  bannerPill: { alignSelf: 'flex-start', backgroundColor: colors.yellow, borderRadius: 5, paddingHorizontal: 6, paddingVertical: 2 },
  bannerPillText: { color: colors.crust, fontWeight: '800', fontSize: 11 },
  bannerText: { color: colors.yellow, fontSize: 14 },
  sheet: { padding: 20, paddingBottom: 40, gap: 10, backgroundColor: colors.base },
  sheetScroll: { backgroundColor: colors.base },
  peekStep: { fontSize: 14, paddingLeft: 88 },
});


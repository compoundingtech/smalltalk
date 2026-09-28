// The contract's control surfaces: an agent's terminal, starting a mission, and the person's
// paired devices. Each takes a fresh fence per action and confirms what cannot be undone.

import { useFocusEffect, useNavigation, useRoute } from '@react-navigation/native';
import { useCallback, useLayoutEffect, useRef, useState } from 'react';
import { AppState, KeyboardAvoidingView, ScrollView, StyleSheet, Text, TextInput, useWindowDimensions, View } from 'react-native';
import { items } from './clientView';
import type { Nav } from './navigation';
import { useStore, type NewMission, type TerminalView } from './store';
import { INTERRUPT_KEY, TERMINAL_KEYS } from './terminalControls';
import { terminalRunStyle } from './terminalStyle';
import { colors } from './theme';
import { Body, Button, Buttons, Card, Dim, Label, ListStateView, MONO, NoticeBar, styles as ui, TextArea } from './ui';

const TERMINAL_COLORS = { fg: colors.text, bg: colors.crust };

/** A terminal screen, sized so its columns fit the phone's width. */
function TerminalLines({ screen, width }: { screen: TerminalView; width: number }) {
  const fontSize = Math.max(7, Math.min(12, (width - 24) / (Math.max(screen.columns, 20) * 0.6)));
  return (
    <Text selectable style={[local.terminal, { fontSize, lineHeight: Math.round(fontSize * 1.3) }]}>
      {screen.lines.map((line, index) => (
        <Text key={line.row ?? index}>
          {line.redacted ? <Text style={{ color: colors.overlay0 }}>[redacted]</Text>
            : line.runs.length ? line.runs.map((run, position) => <Text key={position} style={terminalRunStyle(run, TERMINAL_COLORS)}>{run.text}</Text>)
              : <Text style={{ color: TERMINAL_COLORS.fg }}>{line.text}</Text>}
          {index < screen.lines.length - 1 ? '\n' : ''}
        </Text>
      ))}
    </Text>
  );
}

export function TerminalScreen() {
  const store = useStore();
  const { id } = (useRoute().params ?? {}) as { id?: string };
  const navigation = useNavigation<Nav>();
  const { width } = useWindowDimensions();
  const agent = items(store.world.agents).find(candidate => candidate.id === id);
  const [screen, setScreen] = useState<TerminalView | null>(null);
  const [issue, setIssue] = useState('');
  const [line, setLine] = useState('');
  const session = useRef<ReturnType<typeof store.openTerminal> | null>(null);
  const scroll = useRef<ScrollView>(null);
  const { openTerminal } = store;

  useLayoutEffect(() => { navigation.setOptions({ title: agent ? `${agent.name} · terminal` : 'Terminal' }); }, [navigation, agent?.name]);

  // Attached only while visible and in the foreground; leaving the view detaches.
  useFocusEffect(useCallback(() => {
    if (!agent) return;
    const open = () => { setIssue(''); session.current = openTerminal(agent, { onScreen: setScreen, onIssue: setIssue }); };
    const close = () => { session.current?.close(); session.current = null; };
    open();
    const subscription = AppState.addEventListener('change', state => { if (state === 'active') { if (!session.current) open(); } else close(); });
    return () => { subscription.remove(); close(); };
  }, [agent?.id, openTerminal]));

  if (!agent) return <View style={ui.screen}><ListStateView state={{ kind: 'empty', text: `${id} is not in the current view.` }} /></View>;
  const send = async (mode: 'line' | 'key', value: string) => {
    const ok = await session.current?.send(mode, value);
    if (ok && mode === 'line') setLine('');
  };
  return (
    <View style={ui.screen}>
      <KeyboardAvoidingView behavior="padding" style={local.flex} keyboardVerticalOffset={90}>
        {issue ? <View style={local.issue}><Body color="yellow">{issue}</Body></View> : null}
        <ScrollView ref={scroll} style={local.flex} contentContainerStyle={local.screenBox} contentInsetAdjustmentBehavior="automatic"
          onContentSizeChange={() => scroll.current?.scrollToEnd({ animated: false })}>
          {screen ? <TerminalLines screen={screen} width={width} /> : issue ? null : <ListStateView state={{ kind: 'loading', text: `Opening ${agent.name}'s terminal…` }} />}
        </ScrollView>
        {store.canTypeInTerminals ? (
          <View style={local.controls}>
            <View style={local.lineRow}>
              <TextInput value={line} onChangeText={setLine} placeholder="Type a line" placeholderTextColor={colors.overlay0} autoCapitalize="none" autoCorrect={false}
                keyboardAppearance="dark" style={local.lineInput} onSubmitEditing={() => void send('line', line)} returnKeyType="send" />
              <Button label="Send" filled disabled={!line || !screen} onPress={() => void send('line', line)} />
            </View>
            <Buttons>
              {TERMINAL_KEYS.map(key => <Button key={key.value} label={key.label} color="overlay1" disabled={!screen} onPress={() => void send('key', key.value)} />)}
              <Button label="Ctrl-C" color="red" confirm="Interrupt it" disabled={!screen} onPress={() => void send('key', INTERRUPT_KEY)} />
            </Buttons>
          </View>
        ) : <Dim style={local.readOnly}>This device can read the terminal but not type into it. Pair it with --full-control to send keys.</Dim>}
      </KeyboardAvoidingView>
      <NoticeBar />
    </View>
  );
}

const FIELDS: { key: keyof NewMission; label: string; hint: string; long?: boolean }[] = [
  { key: 'title', label: 'title', hint: "A short name, like 'Nightly dependency audit'" },
  { key: 'request', label: 'what you want', hint: 'Describe the outcome; the planner turns it into a mission', long: true },
  { key: 'mission', label: 'mission id', hint: 'Where it lives in the graph, like fleet/harbor/nightly-audit' },
  { key: 'workspace', label: 'workspace', hint: 'The directory the agents work in' },
];

/** New mission: it creates a launch; the planner's proposal then appears on Home as a launch card. */
export function NewMissionScreen() {
  const store = useStore();
  const navigation = useNavigation<Nav>();
  const [form, setForm] = useState<NewMission>({ title: '', request: '', mission: '', workspace: '' });
  const [missing, setMissing] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const create = async () => {
    const empty = FIELDS.find(field => !form[field.key].trim());
    if (empty) { setMissing(empty.key); return; }
    setBusy(true);
    const ok = await store.createLaunch({ title: form.title.trim(), request: form.request.trim(), mission: form.mission.trim(), workspace: form.workspace.trim() });
    setBusy(false);
    if (ok) navigation.goBack();
  };
  return (
    <View style={ui.screen}>
      <ScrollView contentContainerStyle={ui.scroll} keyboardShouldPersistTaps="handled">
        <Body color="subtext0">Say what you want. The planner turns it into a proposed mission on Home, and nothing runs until you approve it there.</Body>
        {FIELDS.map(field => (
          <View key={field.key} style={local.field}>
            <Label color={missing === field.key ? 'red' : 'accent'}>{missing === field.key ? `fill in ${field.label}` : field.label}</Label>
            {field.long
              ? <TextArea value={form[field.key]} onChange={text => setForm({ ...form, [field.key]: text })} placeholder={field.hint} />
              : <TextInput value={form[field.key]} onChangeText={text => setForm({ ...form, [field.key]: text })} placeholder={field.hint} placeholderTextColor={colors.overlay0}
                autoCapitalize={field.key === 'title' ? 'sentences' : 'none'} autoCorrect={field.key === 'title'} keyboardAppearance="dark" style={local.input} />}
          </View>
        ))}
        <Buttons>
          <Button label="Create the launch" filled disabled={busy} onPress={() => void create()} />
          <Button label="Cancel" color="overlay1" onPress={() => navigation.goBack()} />
        </Buttons>
      </ScrollView>
      <NoticeBar />
    </View>
  );
}

/** The person's paired devices, each revocable after a confirmation. */
export function DevicesCard() {
  const store = useStore();
  const { devices } = store.world;
  return (
    <Card title="your devices">
      {devices.state === 'loading' ? <Dim>Loading your devices…</Dim>
        : devices.state === 'failed' ? <Body color="red">{`Could not read devices: ${devices.value}`}</Body>
          : devices.value.length === 0 ? <Dim>No paired devices.</Dim>
            : devices.value.map(device => (
              <View key={device.id} style={local.device}>
                <View style={local.flex}>
                  <Text style={[local.deviceName, device.state !== 'active' && { color: colors.overlay0 }]}>
                    <Text style={{ color: device.state === 'active' ? colors.green : colors.overlay0 }}>{device.state === 'active' ? '● ' : '○ '}</Text>{device.name}
                  </Text>
                  <Dim>{`${device.scopes.join(', ')} · ${device.state === 'active' ? `expires ${device.expires}` : device.state}`}</Dim>
                </View>
                {device.state === 'active' ? <Button label="Revoke" color="red" confirm="Revoke" onPress={() => void store.revokeDevice(device.id)} /> : null}
              </View>
            ))}
    </Card>
  );
}

const local = StyleSheet.create({
  flex: { flex: 1 },
  issue: { padding: 12, backgroundColor: colors.mantle },
  screenBox: { padding: 12, backgroundColor: colors.crust, flexGrow: 1 },
  terminal: { fontFamily: MONO, color: colors.text },
  controls: { padding: 8, gap: 6, backgroundColor: colors.mantle },
  lineRow: { flexDirection: 'row', alignItems: 'center', gap: 8 },
  lineInput: { flex: 1, color: colors.text, fontFamily: MONO, fontSize: 14, backgroundColor: colors.base, borderRadius: 10, paddingHorizontal: 12, paddingVertical: 9 },
  readOnly: { padding: 12, backgroundColor: colors.mantle },
  field: { gap: 6 },
  input: { color: colors.text, backgroundColor: colors.crust, borderRadius: 10, paddingHorizontal: 12, paddingVertical: 10, fontSize: 15 },
  device: { flexDirection: 'row', alignItems: 'center', gap: 8, paddingVertical: 4 },
  deviceName: { color: colors.text, fontSize: 15, fontWeight: '600' },
});

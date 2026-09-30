import { useCallback, useLayoutEffect, useRef, useState } from 'react';
import { Alert, KeyboardAvoidingView, Platform, ScrollView, StyleSheet, Text, View } from 'react-native';
import { useFocusEffect } from '@react-navigation/native';
import { useHeaderHeight } from '@react-navigation/elements';
import type { TerminalScreen as Screen } from '../../../clients/typescript/st3-client';
import { Banners } from '../chrome';
import type { RootScreen } from '../navigation';
import { errorText, useStore } from '../store';
import { terminalRunStyle } from '../terminalStyle';
import { fonts, theme } from '../theme';
import { Button, Field, Note, T } from '../ui';

const COLORS = { fg: theme.text, bg: theme.crust };
const KEYS = [['return', 'enter'], ['tab', 'tab'], ['escape', 'esc'], ['Up', '↑'], ['Down', '↓']] as const;

// A runtime's live terminal: attached once while visible, followed on the socket, never polled.
export function TerminalScreen({ route, navigation }: RootScreen<'Terminal'>) {
  const { terminalId } = route.params;
  const { feed, status, busy, canControlTerminal, actions } = useStore();
  const [screen, setScreen] = useState<Screen | null>(null);
  const [issue, setIssue] = useState(''), [notice, setNotice] = useState(''), [draft, setDraft] = useState('');
  const incarnation = useRef(''), sending = useRef(false);
  const [sendingNow, setSendingNow] = useState(false);
  const headerHeight = useHeaderHeight();

  useLayoutEffect(() => { navigation.setOptions({ title: route.params.title ? `${route.params.title} · terminal` : 'Terminal' }); }, [navigation, route.params.title]);
  useFocusEffect(useCallback(() => {
    if (!feed) return;
    incarnation.current = '';
    const follow = feed.followTerminal(terminalId, {
      onScreen: next => { if (!incarnation.current) incarnation.current = next.runtime_incarnation; setScreen(next); },
      onIssue: setIssue,
    });
    return () => follow.close();
  }, [feed, terminalId]));

  async function input(mode: 'line' | 'key', value: string) {
    if (status !== 'online' || sending.current || !value || !incarnation.current) return;
    sending.current = true; setSendingNow(true);
    try { await actions.terminalInput(terminalId, incarnation.current, mode, value); if (mode === 'line') setDraft(''); setNotice(''); }
    catch (cause) { setNotice(`Input was not confirmed. Inspect the screen before retrying: ${errorText(cause)}`); }
    finally { sending.current = false; setSendingNow(false); }
  }
  const disabled = busy || sendingNow || status !== 'online' || !!issue;

  return <KeyboardAvoidingView style={{ flex: 1, backgroundColor: theme.crust }} behavior={Platform.OS === 'ios' ? 'padding' : undefined} keyboardVerticalOffset={headerHeight}>
    <Banners />
    <View style={styles.strip}><T dim>{canControlTerminal ? 'live · controls on' : 'live · read-only'}{screen ? ` · ${screen.columns}×${screen.rows}` : ''}</T></View>
    {issue ? <Note tone="warning">{issue}</Note> : null}
    {notice ? <Note tone="warning">{notice}</Note> : null}
    <ScrollView style={{ flex: 1 }} contentContainerStyle={{ padding: 8 }}>
      <ScrollView horizontal>
        <View>
          {screen ? screen.lines.map(line => <Text key={line.row} style={styles.line}>{line.redacted ? '[redacted]' : line.runs.length ? line.runs.map((run, index) => <Text key={index} style={terminalRunStyle(run, COLORS)}>{run.text}</Text>) : line.text || ' '}</Text>)
            : <T dim>{issue || (status === 'online' ? 'Loading terminal screen…' : 'Offline; no terminal screen is cached.')}</T>}
        </View>
      </ScrollView>
    </ScrollView>
    {screen && status !== 'online' ? <Note>offline · showing the last terminal frame</Note> : null}
    {screen && canControlTerminal ? <View style={styles.controls}>
      <View style={{ flexDirection: 'row', gap: 6, alignItems: 'center' }}>
        <T color={theme.accent}>›</T>
        <Field style={{ flex: 1, marginTop: 0 }} autoCapitalize="none" placeholder="a line to type" value={draft} onChangeText={setDraft} onSubmitEditing={() => void input('line', draft)} />
        <Button label="send" style={{ marginTop: 0 }} disabled={disabled || !draft.length} onPress={() => void input('line', draft)} />
      </View>
      <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 6 }}>
        {KEYS.map(([key, label]) => <Button key={key} label={label} disabled={disabled} onPress={() => void input('key', key)} />)}
        <Button label="ctrl-c" color={theme.red} disabled={disabled} onPress={() => Alert.alert('Interrupt terminal process?', 'Send Ctrl-C to this terminal.', [{ text: 'Cancel' }, { text: 'Interrupt', onPress: () => void input('key', 'C-c') }])} />
      </View>
      <T dim style={{ fontSize: 11 }}>inputs need a live connection and are fenced to this terminal's incarnation</T>
    </View> : null}
  </KeyboardAvoidingView>;
}

const styles = StyleSheet.create({
  strip: { paddingHorizontal: 12, paddingVertical: 6, backgroundColor: theme.base },
  line: { color: COLORS.fg, fontFamily: fonts.regular, fontSize: 11, lineHeight: 14 },
  controls: { padding: 10, gap: 4, backgroundColor: theme.mantle, borderTopColor: theme.surface0, borderTopWidth: StyleSheet.hairlineWidth * 2 },
});

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { Alert, InputAccessoryView, KeyboardAvoidingView, PanResponder, Platform, Pressable, ScrollView, StyleSheet, Text, TextInput, View } from 'react-native';
import { useFocusEffect } from '@react-navigation/native';
import { useHeaderHeight } from '@react-navigation/elements';
import type { TerminalScreen as Screen } from '../../../clients/typescript/st3-client';
import { Banners } from '../chrome';
import type { RootScreen } from '../navigation';
import { errorText, useStore } from '../store';
import { InputQueue, controlBytes, keyBytes, plainTyping, rawInput, wheelBytes, type TerminalKey } from '../terminalKeys';
import { terminalRunStyle, withCursor } from '../terminalStyle';
import { fonts, theme } from '../theme';
import { Note, T } from '../ui';

const COLORS = { fg: theme.text, bg: theme.crust };
// IBM Plex Mono advances 600/1000 of an em per character.
const ADVANCE = 0.6;
const PADDING = 4;
// The size the terminal is fitted to when the phone sizes it.
const READABLE = 11;
const BAR = 'terminal-keys';
type BarKey = { label: string; key?: TerminalKey; text?: string };
const KEYS: BarKey[] = [
  { label: 'esc', key: 'escape' }, { label: 'ctrl' }, { label: 'tab', key: 'tab' },
  { label: '←', key: 'left' }, { label: '↓', key: 'down' }, { label: '↑', key: 'up' }, { label: '→', key: 'right' },
  { label: '|', text: '|' }, { label: '~', text: '~' }, { label: '/', text: '/' }, { label: '-', text: '-' }, { label: ':', text: ':' },
  { label: 'pgup', key: 'pageup' }, { label: 'pgdn', key: 'pagedown' },
];

const lineHeightFor = (fontSize: number) => Math.ceil(fontSize * 1.25);

// A runtime's live terminal, typed into the way a terminal app is: tapping the screen brings
// up the keyboard, each key goes to the program as it is pressed, and a bar above the keyboard
// has the keys a phone lacks. Followed on the socket while visible, never polled.
export function TerminalScreen({ route, navigation }: RootScreen<'Terminal'>) {
  const { terminalId } = route.params;
  const { feed, status, canControlTerminal, actions } = useStore();
  const [screen, setScreen] = useState<Screen | null>(null);
  const [issue, setIssue] = useState(''), [notice, setNotice] = useState('');
  const [ctrl, setCtrl] = useState(false), [typing, setTyping] = useState(false);
  const [area, setArea] = useState({ width: 0, height: 0 });
  const incarnation = useRef(''), keyboard = useRef<TextInput>(null), scroller = useRef<ScrollView>(null);
  const live = useRef({ screen, status, ctrl });
  live.current = { screen, status, ctrl };
  const headerHeight = useHeaderHeight();
  const canType = canControlTerminal && status === 'online' && !issue;

  // The store's actions change identity on every render; one queue serves the screen's life,
  // so keys stay in order.
  const latest = useRef(actions);
  latest.current = actions;
  const queue = useMemo(() => new InputQueue(
    bytes => latest.current.terminalInput(terminalId, incarnation.current, 'raw', rawInput(bytes)).then(() => setNotice('')),
    cause => setNotice(`Some keys were not confirmed; look at the screen before typing on: ${errorText(cause)}`),
  ), [terminalId]);
  const send = useCallback((bytes: string) => {
    if (!incarnation.current || live.current.status !== 'online') { setNotice('Offline; keys are not sent.'); return; }
    queue.push(bytes);
  }, [queue]);
  const type = (text: string) => {
    const plain = plainTyping(text);
    if (live.current.ctrl) { setCtrl(false); send(controlBytes(plain)); } else send(plain);
  };
  const press = (bar: BarKey) => {
    if (!bar.key && !bar.text) { setCtrl(on => !on); return; }
    if (bar.text) { type(bar.text); return; }
    setCtrl(false);
    send(keyBytes(bar.key!, live.current.screen?.modes));
  };

  useFocusEffect(useCallback(() => {
    if (!feed) return;
    incarnation.current = '';
    const follow = feed.followTerminal(terminalId, {
      onScreen: next => {
        if (!incarnation.current) {
          incarnation.current = next.runtime_incarnation;
          // Debug builds type these once, for headless checks: the simulator has no keyboard to drive.
          const keys = __DEV__ ? process.env.EXPO_PUBLIC_ST3_TEST_TERMINAL_KEYS : undefined;
          if (keys) setTimeout(() => queue.push(JSON.parse(`"${keys}"`)), 1500);
        }
        setScreen(next);
      },
      onIssue: setIssue,
    });
    return () => follow.close();
  }, [feed, terminalId, queue]));

  // Fit the terminal's columns to the phone's width; a very wide terminal scrolls sideways.
  const columns = screen?.columns ?? 80;
  const fit = area.width ? (area.width - PADDING * 2) / (columns * ADVANCE) : READABLE;
  const fontSize = Math.max(6, Math.min(14, Math.floor(fit * 10) / 10));
  const lineHeight = lineHeightFor(fontSize);
  const contentHeight = (screen?.rows ?? 0) * lineHeight + PADDING * 2;
  const fits = contentHeight <= area.height;

  // Keep the cursor's row in view as the keyboard comes and goes.
  const cursorRow = screen?.cursor.row ?? 0;
  useEffect(() => {
    if (fits || !area.height) return;
    const y = Math.max(0, PADDING + (cursorRow + 1) * lineHeight - area.height + lineHeight * 2);
    scroller.current?.scrollTo({ y, animated: false });
  }, [fits, cursorRow, lineHeight, area.height]);

  const fitToPhone = useCallback(() => {
    if (!incarnation.current || !area.width) return;
    const columns = Math.floor((area.width - PADDING * 2) / (READABLE * ADVANCE));
    const rows = Math.floor((area.height - PADDING * 2) / lineHeightFor(READABLE));
    void latest.current.terminalResize(terminalId, incarnation.current, Math.max(rows, 8), Math.max(columns, 20))
      .catch(cause => setNotice(`The terminal was not resized: ${errorText(cause)}`));
  }, [terminalId, area]);

  useLayoutEffect(() => {
    navigation.setOptions({
      title: route.params.title ? `${route.params.title} · terminal` : 'Terminal',
      unstable_headerRightItems: () => canControlTerminal ? [
        { type: 'menu' as const, label: 'Terminal', icon: { type: 'sfSymbol' as const, name: 'ellipsis.circle' as const }, menu: { items: [
          { type: 'action' as const, label: 'Fit to this phone', onPress: fitToPhone },
          { type: 'action' as const, label: 'Send Ctrl-C', onPress: () => Alert.alert('Interrupt the program?', 'Sends Ctrl-C to this terminal.', [{ text: 'Cancel' }, { text: 'Interrupt', style: 'destructive', onPress: () => send('\x03') }]) },
        ] } },
        { type: 'button' as const, label: 'Keyboard', icon: { type: 'sfSymbol' as const, name: 'keyboard' as const }, onPress: () => (typing ? keyboard.current?.blur() : keyboard.current?.focus()) },
      ] : [],
    });
  }, [navigation, route.params.title, canControlTerminal, fitToPhone, send, typing]);

  // A swipe over a program that reads the mouse turns its wheel; over a full-screen program
  // that does not, it presses the arrows. The plain shell has no history here to scroll.
  const swipe = useRef({ moved: 0 });
  const pan = useMemo(() => PanResponder.create({
    onMoveShouldSetPanResponder: (_, gesture) => {
      const modes = live.current.screen?.modes;
      return !!modes && (modes.mouse_tracking !== 'none' || modes.alternate_screen) && Math.abs(gesture.dy) > 8 && Math.abs(gesture.dy) > Math.abs(gesture.dx);
    },
    onPanResponderGrant: () => { swipe.current.moved = 0; },
    onPanResponderMove: (_, gesture) => {
      const current = live.current.screen;
      if (!current) return;
      const steps = Math.trunc(gesture.dy / lineHeight) - swipe.current.moved;
      if (!steps) return;
      swipe.current.moved += steps;
      // A finger moving up shows what is further down.
      const up = steps > 0;
      const one = current.modes.mouse_tracking !== 'none'
        ? wheelBytes(up, Math.floor(current.columns / 2), Math.floor(current.rows / 2), current.modes)
        : keyBytes(up ? 'up' : 'down', current.modes);
      if (canControlTerminal) send(one.repeat(Math.abs(steps)));
    },
  }), [lineHeight, canControlTerminal, send]);

  const cursor = screen?.cursor.visible ? screen.cursor : undefined;
  const lines = screen?.lines.map(line => {
    const runs = cursor?.row === line.row ? withCursor(line.runs, cursor.column) : line.runs;
    return <Text key={line.row} style={[styles.line, { fontSize, lineHeight, height: lineHeight }]} numberOfLines={1}>
      {line.redacted ? '[redacted]' : runs.length ? runs.map((run, index) => <Text key={index} style={terminalRunStyle(run, COLORS)}>{run.text}</Text>) : ' '}
    </Text>;
  });

  return <KeyboardAvoidingView style={styles.root} behavior={Platform.OS === 'ios' ? 'padding' : undefined} keyboardVerticalOffset={headerHeight}>
    <Banners />
    {issue ? <Note tone="warning">{issue}</Note> : null}
    {notice ? <Note tone="warning">{notice}</Note> : null}
    <View style={styles.root} onLayout={event => setArea({ width: event.nativeEvent.layout.width, height: event.nativeEvent.layout.height })} {...pan.panHandlers}>
      <ScrollView ref={scroller} style={styles.root} scrollEnabled={!fits} bounces={false} keyboardShouldPersistTaps="always">
        <ScrollView horizontal scrollEnabled={fit < 6} bounces={false} keyboardShouldPersistTaps="always">
          <Pressable style={{ padding: PADDING, minHeight: area.height }} onPress={() => (canType ? keyboard.current?.focus() : undefined)}>
            {lines ?? <T dim>{issue || (status === 'online' ? 'Loading the terminal…' : 'Offline; no terminal screen is cached.')}</T>}
          </Pressable>
        </ScrollView>
      </ScrollView>
    </View>
    <View style={styles.strip}>
      <T dim style={styles.stripText}>
        {status !== 'online' ? 'offline · the last frame' : !canControlTerminal ? 'read-only: this phone was paired without terminal control' : typing ? 'typing into the terminal' : 'tap the screen to type'}
        {screen ? ` · ${screen.columns}×${screen.rows}` : ''}
      </T>
      {canType && screen && contentHeight < area.height * 0.8 ? <Pressable onPress={fitToPhone} hitSlop={8}><T style={styles.stripText} color={theme.accent}>fit to phone</T></Pressable> : null}
    </View>
    {canControlTerminal ? <TextInput
      ref={keyboard}
      style={styles.hidden}
      editable={canType}
      autoCapitalize="none"
      autoCorrect={false}
      autoComplete="off"
      spellCheck={false}
      smartInsertDelete={false}
      keyboardType="ascii-capable"
      keyboardAppearance="dark"
      textContentType="none"
      caretHidden
      contextMenuHidden
      submitBehavior="submit"
      inputAccessoryViewID={BAR}
      onFocus={() => setTyping(true)}
      onBlur={() => setTyping(false)}
      // The field stays empty: each change is what was just typed.
      onChangeText={text => { if (text) { type(text); keyboard.current?.clear(); } }}
      onKeyPress={event => { if (event.nativeEvent.key === 'Backspace') press({ label: '⌫', key: 'backspace' }); }}
      onSubmitEditing={() => press({ label: '⏎', key: 'enter' })}
    /> : null}
    {Platform.OS === 'ios' ? <InputAccessoryView nativeID={BAR} backgroundColor={theme.mantle}>
      <View style={styles.bar}>
        <ScrollView horizontal keyboardShouldPersistTaps="always" showsHorizontalScrollIndicator={false} contentContainerStyle={styles.barKeys}>
          {KEYS.map(bar => {
            const on = !bar.key && !bar.text && ctrl;
            return <Pressable key={bar.label} onPress={() => press(bar)} style={({ pressed }) => [styles.key, on && styles.keyOn, pressed && styles.keyPressed]}>
              <Text style={[styles.keyText, on && { color: theme.crust }]}>{bar.label}</Text>
            </Pressable>;
          })}
        </ScrollView>
        <Pressable onPress={() => keyboard.current?.blur()} style={({ pressed }) => [styles.key, pressed && styles.keyPressed]}>
          <Text style={styles.keyText}>⌄</Text>
        </Pressable>
      </View>
    </InputAccessoryView> : null}
  </KeyboardAvoidingView>;
}

const styles = StyleSheet.create({
  root: { flex: 1, backgroundColor: theme.crust },
  strip: { flexDirection: 'row', justifyContent: 'space-between', paddingHorizontal: 12, paddingVertical: 4, backgroundColor: theme.base },
  stripText: { fontSize: 11 },
  line: { color: COLORS.fg, fontFamily: fonts.regular },
  hidden: { position: 'absolute', width: 1, height: 1, opacity: 0, left: -10, top: 0 },
  bar: { flexDirection: 'row', alignItems: 'center', paddingVertical: 6, paddingRight: 6, borderTopColor: theme.surface0, borderTopWidth: StyleSheet.hairlineWidth },
  barKeys: { gap: 6, paddingHorizontal: 6 },
  key: { minWidth: 40, height: 36, paddingHorizontal: 10, borderRadius: 6, alignItems: 'center', justifyContent: 'center', backgroundColor: theme.surface0 },
  keyOn: { backgroundColor: theme.accent },
  keyPressed: { backgroundColor: theme.surface1 },
  keyText: { color: theme.text, fontFamily: fonts.regular, fontSize: 15 },
});

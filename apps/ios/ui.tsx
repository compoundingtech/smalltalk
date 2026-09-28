// Small building blocks every screen shares: named colours only, few borders, selection as a
// fill, glyphs with a legend, and the three list states kept apart.

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { ActivityIndicator, Linking, Pressable, StyleSheet, Text, TextInput, View, type StyleProp, type TextStyle, type ViewStyle } from 'react-native';
import { markdown, type Run } from './markdown';
import type { ListState } from './screenModel';
import { useStore } from './store';
import { colors, type ColorToken } from './theme';

export const MONO = 'Menlo';

export function Label({ children, color = 'overlay1', style }: { children: ReactNode; color?: ColorToken; style?: StyleProp<TextStyle> }) {
  return <Text style={[styles.label, { color: colors[color] }, style]}>{children}</Text>;
}

export function Dim({ children, style, lines }: { children: ReactNode; style?: StyleProp<TextStyle>; lines?: number }) {
  return <Text style={[styles.dim, style]} numberOfLines={lines}>{children}</Text>;
}

export function Body({ children, style, color = 'text', selectable }: { children: ReactNode; style?: StyleProp<TextStyle>; color?: ColorToken; selectable?: boolean }) {
  return <Text selectable={selectable} style={[styles.body, { color: colors[color] }, style]}>{children}</Text>;
}

export function Card({ title, color = 'overlay1', heavy = false, children, style }: { title?: string; color?: ColorToken; heavy?: boolean; children: ReactNode; style?: StyleProp<ViewStyle> }) {
  return (
    <View style={[styles.card, heavy && { borderColor: colors[color], borderWidth: 1 }, style]}>
      {title ? <Label color={color} style={styles.cardTitle}>{title}</Label> : null}
      {children}
    </View>
  );
}

export function Field({ label, children, color = 'subtext0' }: { label: string; children: ReactNode; color?: ColorToken }) {
  return (
    <View style={styles.field}>
      <Text style={styles.fieldLabel}>{label}</Text>
      <Text selectable style={[styles.fieldValue, { color: colors[color] }]}>{children}</Text>
    </View>
  );
}

export function Section({ title, count, color = 'overlay1' }: { title: string; count?: number; color?: ColorToken }) {
  return (
    <View style={styles.section}>
      <Label color={color}>{title}</Label>
      {count !== undefined ? <Text style={styles.sectionCount}>{count}</Text> : null}
    </View>
  );
}

export function Glyph({ glyph, color, size = 15 }: { glyph: string; color: ColorToken; size?: number }) {
  // U+FE0E asks for the text form, so ✉ and friends stay in the palette instead of emoji.
  return <Text style={[styles.glyph, { color: colors[color], fontSize: size }]}>{`${glyph}\uFE0E`}</Text>;
}

export function Pill({ text, color }: { text: string; color: ColorToken }) {
  return <View style={[styles.pill, { backgroundColor: colors[color] }]}><Text style={styles.pillText}>{text}</Text></View>;
}

/** A button. With `confirm`, the first tap arms it and says what a second tap will do. */
export function Button({ label, color = 'accent', onPress, confirm, disabled, filled }: { label: string; color?: ColorToken; onPress: () => void; confirm?: string; disabled?: boolean; filled?: boolean }) {
  const [armed, setArmed] = useState(false);
  useEffect(() => {
    if (!armed) return;
    const timer = setTimeout(() => setArmed(false), 5000);
    return () => clearTimeout(timer);
  }, [armed]);
  const press = () => {
    if (confirm && !armed) { setArmed(true); return; }
    setArmed(false);
    onPress();
  };
  return (
    <Pressable accessibilityRole="button" disabled={disabled} onPress={press}
      style={({ pressed }) => [styles.button, { borderColor: colors[color] }, (filled || armed) && { backgroundColor: colors[color] }, pressed && styles.pressed, disabled && styles.disabled]}>
      <Text style={[styles.buttonText, { color: filled || armed ? colors.crust : colors[color] }]}>{armed ? `${confirm}? Tap again` : label}</Text>
    </Pressable>
  );
}

export function Buttons({ children }: { children: ReactNode }) {
  return <View style={styles.buttons}>{children}</View>;
}

/** Glyphs explained once, instead of words like "reachable" on every row. */
export function Legend({ entries }: { entries: [string, ColorToken, string][] }) {
  return (
    <View style={styles.legend}>
      {entries.map(([glyph, color, word]) => (
        <View key={word} style={styles.legendEntry}><Glyph glyph={glyph} color={color} size={12} /><Dim style={styles.legendWord}>{word}</Dim></View>
      ))}
    </View>
  );
}

/** Loading, empty and failed: three different things, never an empty list while loading. */
export function ListStateView({ state, retry }: { state: ListState; retry?: () => void }) {
  if (state.kind === 'ready') return null;
  return (
    <View style={styles.state}>
      {state.kind === 'loading' ? <ActivityIndicator color={colors.overlay1} /> : null}
      <Body color={state.kind === 'failed' ? 'red' : state.kind === 'empty' ? 'subtext0' : 'overlay1'} style={styles.stateText}>{state.kind === 'failed' ? `Could not load: ${state.text}` : state.text}</Body>
      {state.kind === 'failed' && retry ? <Buttons><Button label="Try again" onPress={retry} /></Buttons> : null}
    </View>
  );
}

export function Segmented<T extends string>({ options, value, onChange }: { options: [T, string][]; value: T; onChange: (value: T) => void }) {
  return (
    <View style={styles.segmented} accessibilityRole="tablist">
      {options.map(([key, label]) => (
        <Pressable key={key} accessibilityRole="tab" accessibilityState={{ selected: key === value }} onPress={() => onChange(key)} style={[styles.segment, key === value && styles.segmentOn]}>
          <Text style={[styles.segmentText, key === value && styles.segmentTextOn]}>{label}</Text>
        </Pressable>
      ))}
    </View>
  );
}

/** A growing text box, up to eight lines. */
export function Composer({ value, onChange, placeholder, onSend, sendLabel = 'Send', disabled, autoFocus }: { value: string; onChange: (text: string) => void; placeholder: string; onSend: () => void; sendLabel?: string; disabled?: boolean; autoFocus?: boolean }) {
  return (
    <View style={styles.composer}>
      <TextInput multiline autoFocus={autoFocus} value={value} onChangeText={onChange} placeholder={placeholder} placeholderTextColor={colors.overlay0}
        style={styles.composerInput} keyboardAppearance="dark" />
      <Pressable accessibilityRole="button" disabled={disabled || !value.trim()} onPress={onSend} style={[styles.send, (disabled || !value.trim()) && styles.disabled]}>
        <Text style={styles.sendText}>{sendLabel}</Text>
      </Pressable>
    </View>
  );
}

/** A growing text box without its own send button, for a card's notes. */
export function TextArea({ value, onChange, placeholder, autoFocus }: { value: string; onChange: (text: string) => void; placeholder: string; autoFocus?: boolean }) {
  return <TextInput multiline autoFocus={autoFocus} value={value} onChangeText={onChange} placeholder={placeholder} placeholderTextColor={colors.overlay0} style={[styles.composerInput, styles.textArea]} keyboardAppearance="dark" />;
}

/** The last thing that happened, for a few seconds. */
export function NoticeBar() {
  const store = useStore();
  const { notice, clearNotice } = store;
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(clearNotice, 4500);
    return () => clearTimeout(timer);
  }, [notice, clearNotice]);
  if (!notice) return null;
  const bad = /^(Failed|Not sent|st does not)/.test(notice);
  return (
    <Pressable onPress={clearNotice} style={[styles.notice, bad && { borderColor: colors.red }]}>
      <Text style={[styles.noticeText, bad && { color: colors.red }]}>{notice}</Text>
    </Pressable>
  );
}

/** Demo mode says so on every screen. */
export function DemoBadge() {
  const store = useStore();
  if (store.mode !== 'demo') return null;
  return <View style={styles.demo}><Text style={styles.demoText}>DEMO</Text></View>;
}

// ------------------------------------------------------------------- markdown

function Runs({ runs, color }: { runs: Run[]; color: ColorToken }) {
  return <>{runs.map((run, index) => (
    <Text key={index} onPress={run.link ? () => { void Linking.openURL(run.link!); } : undefined}
      style={[run.bold && styles.bold, run.code && styles.code, run.link && styles.link, !run.code && !run.link && { color: colors[color] }]}>{run.text}</Text>
  ))}</>;
}

/** Markdown the way pi renders a reply: real headings, bullets, code set off, quotes, tables. */
export function Markdown({ text, color = 'text', style }: { text: string; color?: ColorToken; style?: StyleProp<ViewStyle> }) {
  const blocks = markdown(text);
  return (
    <View style={style}>
      {blocks.map((block, index) => {
        switch (block.kind) {
          case 'blank': return <View key={index} style={styles.blank} />;
          case 'rule': return <View key={index} style={styles.rule} />;
          case 'heading': return <Text key={index} selectable style={[styles.heading, block.level === 1 && styles.heading1]}><Runs runs={block.runs} color="peach" /></Text>;
          case 'quote': return <View key={index} style={styles.quote}><Text selectable style={styles.quoteText}><Runs runs={block.runs} color="subtext0" /></Text></View>;
          case 'item': return (
            <View key={index} style={[styles.item, { paddingLeft: block.indent * 6 }]}>
              <Text style={styles.marker}>{block.marker}</Text>
              <Text selectable style={[styles.body, styles.itemText, { color: colors[color] }]}><Runs runs={block.runs} color={color} /></Text>
            </View>
          );
          case 'code': return (
            <View key={index} style={styles.codeBlock}>
              {block.language ? <Dim style={styles.codeLanguage}>{block.language}</Dim> : null}
              <Text selectable style={styles.codeText}>{block.lines.join('\n')}</Text>
            </View>
          );
          case 'table': return (
            <View key={index} style={styles.table}>
              {block.rows.map((row, rowIndex) => (
                <View key={rowIndex} style={[styles.tableRow, rowIndex === 0 && styles.tableHead]}>
                  {row.map((cell, cellIndex) => <Text key={cellIndex} selectable style={[styles.tableCell, rowIndex === 0 && styles.bold, { color: colors[color] }]}>{cell}</Text>)}
                </View>
              ))}
            </View>
          );
          case 'text': return <Text key={index} selectable style={[styles.body, { color: colors[color] }]}><Runs runs={block.runs} color={color} /></Text>;
        }
      })}
    </View>
  );
}

export const styles = StyleSheet.create({
  screen: { flex: 1, backgroundColor: colors.base },
  scroll: { padding: 16, paddingBottom: 48, gap: 12 },
  label: { fontSize: 12, fontWeight: '700', textTransform: 'lowercase', letterSpacing: 0.3 },
  dim: { color: colors.overlay0, fontSize: 13 },
  body: { color: colors.text, fontSize: 15, lineHeight: 21 },
  bold: { fontWeight: '700' },
  code: { fontFamily: MONO, color: colors.teal, fontSize: 14 },
  link: { color: colors.blue, textDecorationLine: 'underline' },
  card: { backgroundColor: colors.mantle, borderRadius: 12, padding: 14, gap: 8 },
  cardTitle: { marginBottom: 2 },
  field: { flexDirection: 'row', gap: 10 },
  fieldLabel: { color: colors.overlay0, fontSize: 13, width: 78, paddingTop: 1 },
  fieldValue: { flex: 1, fontSize: 14, lineHeight: 20 },
  section: { flexDirection: 'row', alignItems: 'baseline', gap: 8, paddingTop: 16, paddingBottom: 6, paddingHorizontal: 16, backgroundColor: colors.base },
  sectionCount: { color: colors.overlay0, fontSize: 12 },
  glyph: { fontWeight: '700', width: 18, textAlign: 'center' },
  pill: { alignSelf: 'flex-start', borderRadius: 6, paddingHorizontal: 8, paddingVertical: 3 },
  pillText: { color: colors.crust, fontWeight: '800', fontSize: 12 },
  button: { borderWidth: 1, borderRadius: 9, paddingHorizontal: 12, paddingVertical: 8 },
  buttonText: { fontWeight: '700', fontSize: 14 },
  buttons: { flexDirection: 'row', flexWrap: 'wrap', gap: 8, marginTop: 4 },
  pressed: { opacity: 0.6 },
  disabled: { opacity: 0.4 },
  legend: { flexDirection: 'row', flexWrap: 'wrap', gap: 12, paddingHorizontal: 16, paddingVertical: 10 },
  legendEntry: { flexDirection: 'row', alignItems: 'center', gap: 2 },
  legendWord: { fontSize: 12 },
  state: { padding: 24, gap: 10, alignItems: 'center' },
  stateText: { textAlign: 'center' },
  segmented: { flexDirection: 'row', backgroundColor: colors.mantle, borderRadius: 9, padding: 2, marginHorizontal: 16, marginVertical: 8 },
  segment: { flex: 1, paddingVertical: 6, borderRadius: 7, alignItems: 'center' },
  segmentOn: { backgroundColor: colors.surface1 },
  segmentText: { color: colors.subtext0, fontWeight: '600', fontSize: 13 },
  segmentTextOn: { color: colors.text },
  composer: { flexDirection: 'row', alignItems: 'flex-end', gap: 8, padding: 8, backgroundColor: colors.mantle },
  composerInput: { flex: 1, color: colors.text, fontSize: 15, lineHeight: 20, maxHeight: 8 * 20 + 16, minHeight: 36, backgroundColor: colors.base, borderRadius: 10, paddingHorizontal: 12, paddingTop: 8, paddingBottom: 8 },
  textArea: { flex: 0, backgroundColor: colors.crust },
  send: { backgroundColor: colors.accent, borderRadius: 10, paddingHorizontal: 14, paddingVertical: 9 },
  sendText: { color: colors.crust, fontWeight: '800' },
  notice: { position: 'absolute', left: 12, right: 12, bottom: 12, backgroundColor: colors.surface0, borderRadius: 10, padding: 12, borderWidth: 1, borderColor: colors.surface1 },
  noticeText: { color: colors.text, fontSize: 14 },
  demo: { backgroundColor: colors.yellow, borderRadius: 5, paddingHorizontal: 6, paddingVertical: 2 },
  demoText: { color: colors.crust, fontWeight: '800', fontSize: 11 },
  blank: { height: 8 },
  rule: { height: 1, backgroundColor: colors.surface1, marginVertical: 8 },
  heading: { color: colors.peach, fontWeight: '700', fontSize: 16, lineHeight: 22 },
  heading1: { fontSize: 18, textDecorationLine: 'underline' },
  quote: { borderLeftWidth: 2, borderLeftColor: colors.surface2, paddingLeft: 10 },
  quoteText: { color: colors.subtext0, fontStyle: 'italic', fontSize: 15, lineHeight: 21 },
  item: { flexDirection: 'row' },
  marker: { color: colors.lavender, fontSize: 15, lineHeight: 21 },
  itemText: { flex: 1 },
  codeBlock: { backgroundColor: colors.crust, borderRadius: 8, padding: 10, marginVertical: 2 },
  codeLanguage: { fontSize: 11, marginBottom: 4 },
  codeText: { fontFamily: MONO, color: colors.subtext1, fontSize: 13, lineHeight: 18 },
  table: { borderRadius: 8, backgroundColor: colors.crust, paddingVertical: 4 },
  tableRow: { flexDirection: 'row', paddingHorizontal: 8, paddingVertical: 3 },
  tableHead: { borderBottomWidth: 1, borderBottomColor: colors.surface1 },
  tableCell: { flex: 1, fontSize: 13 },
  row: { flexDirection: 'row', alignItems: 'center', gap: 8, paddingHorizontal: 16, paddingVertical: 10 },
  rowMain: { flex: 1, gap: 2 },
  rowTitle: { color: colors.text, fontSize: 15, fontWeight: '600' },
  rowRight: { alignItems: 'flex-end', gap: 2 },
});

/** Keep a scroll position stable per key while the screen lives. */
export function useLatest<T>(value: T) {
  const ref = useRef(value);
  ref.current = value;
  return ref;
}

import { createContext, useContext, useCallback, useRef, type ReactNode, type Ref } from 'react';
import { quality } from './qualityNative';
import * as Clipboard from 'expo-clipboard';
import { ActionSheetIOS, Linking, Pressable, StyleSheet, Text, TextInput, View, useColorScheme, type StyleProp, type TextInputProps, type TextStyle, type ViewStyle } from 'react-native';
import { fonts, systemColors, theme } from './theme';
import type { Run } from './markdown';
import { markdown } from './markdown';

// Shared renderers retain the legacy stui presentation by default. SystemScreen scopes
// system text, semantic appearance and grouped rows to the migrated native surfaces.

export const SIZE = 14;
export const LINE = 20;

const SystemAppearance = createContext(false);
export function useSystemColors() {
  return systemColors[useColorScheme() === 'dark' ? 'dark' : 'light'];
}
export function useTextAppearance() {
  const system = useContext(SystemAppearance);
  const colors = useSystemColors();
  const color = (value?: string) => {
    if (!system) return value;
    if (!value || value === theme.text) return colors.text;
    if (value === theme.person || value === theme.waiting || value === theme.working || value === theme.yellow || value === theme.peach) return colors.attention;
    if (value === theme.red) return colors.fault;
    if (value === theme.green || value === theme.teal) return colors.success;
    if (value === theme.blue || value === theme.lavender || value === theme.sapphire) return colors.action;
    return colors.secondary;
  };
  return { system, colors, color };
}

/** Opt-in system typography and appearance, scoped to one migrated screen. */
export function SystemScreen({ children }: { children: ReactNode }) {
  const colors = useSystemColors();
  return <SystemAppearance.Provider value={true}><View style={{ flex: 1, backgroundColor: colors.background }}>{children}</View></SystemAppearance.Provider>;
}

/** Wrapping, inset grouped row. The label carries meaning without a glyph legend. */
export function GroupedRow({ title, context, onPress, onLongPress }: { title: string; context: string; onPress: () => void; onLongPress?: () => void }) {
  const colors = useSystemColors();
  return <Pressable accessibilityRole="button" accessibilityLabel={`${title}. ${context}`} accessibilityHint="Opens alert details" onPress={onPress} onLongPress={onLongPress}
    style={({ pressed }) => ({ marginHorizontal: 16, padding: 16, minHeight: 60, borderRadius: 10, backgroundColor: pressed ? colors.separator : colors.grouped, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: colors.separator, flexDirection: 'row', alignItems: 'center', gap: 12 })}>
    <View style={{ flex: 1, gap: 6 }}><T bold>{title}</T><T dim>{context}</T></View>
    <Text accessible={false} style={{ color: colors.secondary, fontSize: 22 }}>›</Text>
  </Pressable>;
}

export function GroupedHeader({ title, count }: { title: string; count?: number }) {
  return <View style={{ paddingHorizontal: 32, paddingTop: 24, paddingBottom: 8 }} accessibilityRole="header"><T bold>{title}{count === undefined ? '' : ` · ${count}`}</T></View>;
}

export function T({ children, style, color, dim, bold, soft, numberOfLines, selectable }: { children: ReactNode; style?: StyleProp<TextStyle>; color?: string; dim?: boolean; bold?: boolean; soft?: boolean; numberOfLines?: number; selectable?: boolean }) {
  const appearance = useTextAppearance();
  return <Text allowFontScaling numberOfLines={numberOfLines} selectable={selectable} style={[styles.text, dim && { color: theme.overlay0 }, soft && { color: theme.subtext0 }, bold && styles.bold, color ? { color } : null, appearance.system && { fontFamily: 'System', fontSize: 17, lineHeight: undefined, fontWeight: bold ? '600' : '400', color: appearance.color(color ?? (dim || soft ? theme.subtext0 : theme.text)) }, style]}>{children}</Text>;
}

/** A section rule: `idle ────── 19`, mauve when a person is needed. */
export function SectionHeader({ title, count, color = theme.overlay1 }: { title: string; count?: number; color?: string }) {
  return <View style={styles.section} accessibilityRole="header">
    <T bold color={color}>{title}</T>
    <View style={[styles.rule, { backgroundColor: theme.surface1 }]} />
    {count === undefined ? null : <T dim>{count}</T>}
  </View>;
}

/** One list row: glyph, name, right-hand detail, and an optional dim second line. */
export function ListRow({ glyph, glyphColor, title, right, second, onPress, onLongPress, indent = 0, selected, accessibilityLabel }: { glyph?: string; glyphColor?: string; title: ReactNode; right?: ReactNode; second?: ReactNode; onPress?: () => void; onLongPress?: () => void; indent?: number; selected?: boolean; accessibilityLabel?: string }) {
  return <Pressable accessibilityRole={onPress ? 'button' : undefined} accessibilityLabel={accessibilityLabel} disabled={!onPress && !onLongPress} onPress={onPress} onLongPress={onLongPress} style={({ pressed }) => [styles.row, (pressed || selected) && { backgroundColor: theme.rowSelected }, { paddingLeft: 12 + indent * 16 }]}>
    <View style={styles.rowFirst}>
      {glyph ? <T bold color={glyphColor} style={styles.glyph}>{glyph}</T> : null}
      <View style={styles.rowTitle}>{typeof title === 'string' ? <T bold numberOfLines={1}>{title}</T> : title}</View>
      {right ? <View style={styles.rowRight}>{right}</View> : null}
    </View>
    {second ? <View style={glyph ? styles.rowSecondIndented : styles.rowSecond}>{typeof second === 'string' ? <T dim numberOfLines={1}>{second}</T> : second}</View> : null}
  </Pressable>;
}

/** A legend under a list: glyph and word, three to a line. */
export function Legend({ entries }: { entries: ReadonlyArray<{ glyph: string; color: string; word: string }> }) {
  return <View style={styles.legend}>{entries.map(entry => <View key={entry.word} style={styles.legendEntry}><T color={entry.color}>{entry.glyph} </T><T dim>{entry.word}</T></View>)}</View>;
}

export function Note({ children, tone = 'dim' }: { children: ReactNode; tone?: 'dim' | 'warning' | 'fault' }) {
  return <T style={styles.note} color={tone === 'warning' ? theme.waiting : tone === 'fault' ? theme.fault : theme.overlay0}>{children}</T>;
}

export function Button({ label, onPress, disabled = false, color = theme.accent, style }: { label: string; onPress: () => void; disabled?: boolean; color?: string; style?: StyleProp<ViewStyle> }) {
  const appearance = useTextAppearance();
  return <Pressable accessibilityRole="button" accessibilityLabel={label} accessibilityState={{ disabled }} disabled={disabled} onPress={onPress} style={({ pressed }) => [styles.button, appearance.system && { minHeight: 44, justifyContent: 'center', backgroundColor: appearance.colors.grouped, paddingHorizontal: 16, paddingVertical: 12, borderRadius: 10 }, pressed && (appearance.system ? { opacity: 0.7 } : { backgroundColor: theme.surface1 }), disabled && styles.disabled, style]}>
    <T bold color={color}>{label}</T>
  </Pressable>;
}

/** A text input. Prose (messages, titles, requests) gets iOS's autocorrect and spell check;
 *  a field for an identifier (a URL, a code, a path) turns them off. */
export function Field(props: TextInputProps & { ref?: Ref<TextInput> }) {
  const keyHandler = useRef(props.onKeyPress); keyHandler.current = props.onKeyPress;
  const onKeyPress = useCallback<NonNullable<TextInputProps['onKeyPress']>>(event => { quality.key(); keyHandler.current?.(event); }, []);
  return <TextInput placeholderTextColor={theme.overlay0} autoCorrect spellCheck {...props} onKeyPress={onKeyPress} style={[styles.field, props.multiline && styles.fieldMultiline, props.style]} />;
}

/** A banner under the header: a problem to read, tap to dismiss when it can be. */
export function Banner({ text, tone = 'fault', onPress }: { text: string; tone?: 'fault' | 'warning' | 'quiet'; onPress?: () => void }) {
  const appearance = useTextAppearance();
  const color = tone === 'fault' ? theme.fault : tone === 'warning' ? theme.waiting : theme.subtext0;
  return <Pressable accessibilityRole={onPress ? 'button' : undefined} disabled={!onPress} onPress={onPress} style={[styles.banner, { borderLeftColor: appearance.color(color), backgroundColor: appearance.system ? appearance.colors.grouped : theme.mantle }]}><T color={color}>{text}</T></Pressable>;
}

export function Screen({ children, style }: { children: ReactNode; style?: StyleProp<ViewStyle> }) {
  return <View style={[styles.screen, style]}>{children}</View>;
}

function runStyle(run: Run, base: TextStyle): StyleProp<TextStyle> {
  switch (run.style) {
    case 'bold': return [base, styles.bold];
    case 'code': return [base, { color: theme.teal }];
    case 'link': return [base, { color: theme.blue, textDecorationLine: 'underline' }];
    default: return base;
  }
}
// A path in a message names a file on the writer's machine: say so, and offer to copy it.
function showPath(path: string) {
  ActionSheetIOS.showActionSheetWithOptions(
    { title: 'A file on the writer’s machine', message: path, options: ['Copy path', 'Cancel'], cancelButtonIndex: 1 },
    index => { if (index === 0) void Clipboard.setStringAsync(path); },
  );
}
function Runs({ runs, base }: { runs: Run[]; base: TextStyle }) {
  const appearance = useTextAppearance();
  return <>{runs.map((run, index) => <Text key={index} style={appearance.system ? [base, run.style === 'bold' && { fontWeight: '600' }, run.style === 'code' && { fontFamily: fonts.regular }, run.style === 'link' && { color: appearance.colors.action, textDecorationLine: 'underline' }] : runStyle(run, base)} onPress={run.url ? () => void Linking.openURL(run.url!) : run.path ? () => showPath(run.path!) : undefined}>{run.text}</Text>)}</>;
}

/** Markdown as stui renders a reply: peach headings, lavender bullets, code in a gutter. */
export function Markdown({ text, color = theme.text, selectable = true }: { text: string; color?: string; selectable?: boolean }) {
  const appearance = useTextAppearance();
  const base: TextStyle = appearance.system ? { fontFamily: 'System', fontSize: 17, color: appearance.color(color) } : { ...styles.text, color };
  return <View>{markdown(text).map((block, index) => {
    switch (block.kind) {
      case 'blank': return <View key={index} style={{ height: LINE / 2 }} />;
      case 'heading': return <Text key={index} style={[base, appearance.system ? { fontWeight: '600', color: appearance.color(theme.peach) } : [styles.bold, { color: theme.peach }, block.level === 1 && { textDecorationLine: 'underline' }]]}><Runs runs={block.runs} base={appearance.system ? { ...base, fontWeight: '600' } : { ...base, ...styles.bold, color: theme.peach }} /></Text>;
      case 'rule': return <View key={index} style={[styles.mdRule, { backgroundColor: theme.surface2 }]} />;
      case 'quote': return <View style={styles.quote}><Text style={[base, appearance.system ? { color: appearance.colors.secondary, fontStyle: 'italic' } : { color: theme.subtext0, fontFamily: fonts.italic }]}><Runs runs={block.runs} base={appearance.system ? base : { ...base, color: theme.subtext0, fontFamily: fonts.italic }} /></Text></View>;
      case 'item': return <View style={[styles.item, { paddingLeft: block.indent * 7 }]}><Text style={[base, { color: appearance.color(theme.lavender) }]}>{block.marker}</Text><Text style={[base, styles.itemText]}><Runs runs={block.runs} base={base} /></Text></View>;
      case 'fence': return <Text key={index} style={[base, { color: theme.overlay0 }]}>{block.text}</Text>;
      case 'code': return <Text key={index} selectable={selectable} style={[base, { fontFamily: fonts.regular, color: appearance.color(theme.subtext1), paddingLeft: 14 }]}>{block.text || ' '}</Text>;
      case 'table': return <Text key={index} style={[base, !appearance.system && { color: theme.subtext0 }]}>{block.text}</Text>;
      case 'text': return <Text key={index} selectable={selectable} style={base}><Runs runs={block.runs} base={base} /></Text>;
    }
  })}</View>;
}

export const styles = StyleSheet.create({
  screen: { flex: 1, backgroundColor: theme.base },
  text: { fontFamily: fonts.regular, fontSize: SIZE, lineHeight: LINE, color: theme.text },
  bold: { fontFamily: fonts.bold },
  section: { flexDirection: 'row', alignItems: 'center', gap: 8, paddingHorizontal: 12, paddingTop: 14, paddingBottom: 4 },
  rule: { flex: 1, height: StyleSheet.hairlineWidth * 2 },
  row: { paddingRight: 12, paddingVertical: 5 },
  rowFirst: { flexDirection: 'row', alignItems: 'center' },
  glyph: { width: 22 },
  rowTitle: { flex: 1, minWidth: 0 },
  rowRight: { flexDirection: 'row', alignItems: 'center', gap: 10, marginLeft: 8 },
  rowSecond: {},
  rowSecondIndented: { paddingLeft: 22 },
  legend: { flexDirection: 'row', flexWrap: 'wrap', paddingHorizontal: 12, paddingVertical: 12, rowGap: 2 },
  legendEntry: { flexDirection: 'row', width: '33%' },
  note: { paddingHorizontal: 12, paddingVertical: 6 },
  button: { alignSelf: 'flex-start', paddingHorizontal: 10, paddingVertical: 6, borderRadius: 4, backgroundColor: theme.surface0, marginTop: 8 },
  disabled: { opacity: 0.4 },
  field: { fontFamily: fonts.regular, fontSize: SIZE, color: theme.text, backgroundColor: theme.mantle, borderColor: theme.surface1, borderWidth: StyleSheet.hairlineWidth * 2, borderRadius: 4, paddingHorizontal: 10, paddingVertical: 8, marginTop: 8 },
  fieldMultiline: { minHeight: 88, textAlignVertical: 'top' },
  banner: { backgroundColor: theme.mantle, borderLeftWidth: 3, paddingHorizontal: 12, paddingVertical: 8 },
  mdRule: { height: 1, marginVertical: 8 },
  quote: { borderLeftWidth: 2, borderLeftColor: theme.surface2, paddingLeft: 8 },
  item: { flexDirection: 'row' },
  itemText: { flex: 1 },
});

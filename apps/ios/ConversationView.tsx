// A conversation drawn the way pi draws one: the person's messages as tinted blocks, replies
// as Markdown, tool calls as small boxes tinted by outcome and collapsed to their last lines,
// Small Talk mail in the same stream. An unchanged entry never re-renders, new entries only
// append, and the view follows the end only while the reader is at the end.

import { memo, useCallback, useEffect, useRef, useState } from 'react';
import { Pressable, ScrollView, StyleSheet, Text, View, type NativeScrollEvent, type NativeSyntheticEvent } from 'react-native';
import type { Entry, Load } from './clientView';
import { colors } from './theme';
import { Body, Dim, ListStateView, Markdown, MONO } from './ui';
import { SPINNER } from './words';

const COLLAPSED_TOOL_LINES = 5;

function ToolBox({ entry }: { entry: Extract<Entry['body'], { kind: 'tool' }>['value'] }) {
  const [open, setOpen] = useState(false);
  const { title, state, output } = entry;
  const [bg, glyph, color] = state === 'running' ? [colors.tool_bg, SPINNER[0], colors.working] : state === 'ok' ? [colors.tool_ok_bg, '✓', colors.green] : [colors.tool_err_bg, '✕', colors.red];
  const everything = state === 'failed' || output.length <= COLLAPSED_TOOL_LINES || open;
  const shown = everything ? output : output.slice(-COLLAPSED_TOOL_LINES);
  const hidden = output.length - shown.length;
  return (
    <View style={[styles.tool, { backgroundColor: bg }]}>
      <Pressable onPress={() => setOpen(!open)} accessibilityRole="button" accessibilityLabel={`${title}, ${state}`}>
        <View style={styles.toolHead}>
          <Text style={[styles.toolGlyph, { color }]}>{glyph}</Text>
          <Text style={styles.toolTitle} numberOfLines={open ? undefined : 1}>{title}</Text>
        </View>
        {hidden > 0 ? <Dim style={styles.toolMore}>… {hidden} earlier lines · tap to expand</Dim> : null}
      </Pressable>
      {shown.length ? (
        <Text selectable style={styles.toolOutput}>
          {shown.map((line, index) => (
            <Text key={index} style={{ color: line.startsWith('+') ? colors.green : line.startsWith('-') || line.includes('error') ? colors.red : colors.subtext0 }}>{line}{index < shown.length - 1 ? '\n' : ''}</Text>
          ))}
        </Text>
      ) : null}
      {open && output.length > COLLAPSED_TOOL_LINES ? <Pressable onPress={() => setOpen(false)}><Dim style={styles.toolMore}>collapse</Dim></Pressable> : null}
    </View>
  );
}

function Bar({ color, children }: { color: string; children: React.ReactNode }) {
  return <View style={[styles.bar, { borderLeftColor: color }]}>{children}</View>;
}

export const EntryView = memo(function EntryView({ entry }: { entry: Entry }) {
  const { body, at } = entry;
  switch (body.kind) {
    case 'user':
      return (
        <View style={styles.user}>
          {at ? <Dim style={styles.userTime}>{at}</Dim> : null}
          <Body selectable>{body.value}</Body>
        </View>
      );
    case 'assistant':
      return <Markdown text={body.value} style={styles.assistant} />;
    case 'thinking':
      return <Text selectable style={styles.thinking}>∴ {body.value}</Text>;
    case 'tool':
      return <ToolBox entry={body.value} />;
    case 'mail':
      return (
        <Bar color={colors.sapphire}>
          <Text style={styles.mailHead}>
            <Text style={styles.mailWho}>{body.value.from} → {body.value.to}</Text>
            {body.value.subject ? <Text style={styles.mailSubject}>  {body.value.subject}</Text> : null}
            {at ? <Text style={styles.mailAt}>  {at}</Text> : null}
          </Text>
          <Markdown text={body.value.body} color="subtext0" />
        </Bar>
      );
    case 'pending':
      return (
        <Bar color={body.value.failed ? colors.red : colors.surface2}>
          <Text style={[styles.mailHead, { color: body.value.failed ? colors.red : colors.overlay0 }]}>
            {body.value.failed ? `you · not sent: ${body.value.failed}` : 'you · sending…'}{at ? `  ${at}` : ''}
          </Text>
          <Markdown text={body.value.text} color="overlay0" />
        </Bar>
      );
    case 'event':
      return (
        <View style={styles.event}>
          <View style={styles.eventRule} />
          <Dim style={styles.eventText}>{body.value}{at ? ` · ${at}` : ''}</Dim>
        </View>
      );
  }
}, (a, b) => a.entry.id === b.entry.id && a.entry.at === b.entry.at && JSON.stringify(a.entry.body) === JSON.stringify(b.entry.body));

export function EntryList({ entries }: { entries: Entry[] }) {
  return <View style={styles.entries}>{entries.map(entry => <EntryView key={entry.id} entry={entry} />)}</View>;
}

/** The whole stream for one agent, following the end while the reader is there. */
export function ConversationView({ load, empty, header }: { load: Load<Entry[]> | undefined; empty: string; header?: React.ReactNode }) {
  const scroll = useRef<ScrollView>(null);
  const atEnd = useRef(true);
  const seen = useRef(0);
  const [fresh, setFresh] = useState(0);
  const entries = load?.state === 'ready' ? load.value : [];

  const onScroll = useCallback((event: NativeSyntheticEvent<NativeScrollEvent>) => {
    const { contentOffset, layoutMeasurement, contentSize } = event.nativeEvent;
    atEnd.current = contentOffset.y + layoutMeasurement.height >= contentSize.height - 48;
    if (atEnd.current) { seen.current = entries.length; setFresh(0); }
  }, [entries.length]);

  useEffect(() => {
    if (atEnd.current) seen.current = entries.length;
    else setFresh(Math.max(0, entries.length - seen.current));
  }, [entries.length]);

  const jump = () => { scroll.current?.scrollToEnd({ animated: true }); atEnd.current = true; seen.current = entries.length; setFresh(0); };

  return (
    <View style={styles.flex}>
      <ScrollView ref={scroll} style={styles.flex} contentContainerStyle={styles.stream} onScroll={onScroll} scrollEventThrottle={64}
        keyboardDismissMode="interactive" contentInsetAdjustmentBehavior="automatic"
        onContentSizeChange={() => { if (atEnd.current) scroll.current?.scrollToEnd({ animated: false }); }}>
        {header}
        {load === undefined || load.state === 'loading' ? <ListStateView state={{ kind: 'loading', text: 'Loading the conversation…' }} />
          : load.state === 'failed' ? <View style={styles.why}><Body color="subtext0">{load.value}</Body></View>
            : entries.length === 0 ? <ListStateView state={{ kind: 'empty', text: empty }} />
              : <EntryList entries={entries} />}
      </ScrollView>
      {fresh > 0 ? (
        <Pressable onPress={jump} style={styles.chip} accessibilityRole="button">
          <Text style={styles.chipText}>↓ {fresh} new</Text>
        </Pressable>
      ) : null}
    </View>
  );
}

const styles = StyleSheet.create({
  flex: { flex: 1 },
  stream: { padding: 12, paddingBottom: 24 },
  entries: { gap: 12 },
  why: { padding: 16, backgroundColor: colors.mantle, borderRadius: 12 },
  user: { backgroundColor: colors.user_bg, borderRadius: 10, padding: 12, gap: 4 },
  userTime: { alignSelf: 'flex-end', fontSize: 11 },
  assistant: { paddingHorizontal: 4 },
  thinking: { color: colors.overlay1, fontStyle: 'italic', fontSize: 14, lineHeight: 20, paddingHorizontal: 4 },
  tool: { borderRadius: 8, padding: 10, gap: 4 },
  toolHead: { flexDirection: 'row', gap: 8, alignItems: 'center' },
  toolGlyph: { fontWeight: '800', fontSize: 14 },
  toolTitle: { flex: 1, color: colors.text, fontWeight: '700', fontFamily: MONO, fontSize: 13 },
  toolMore: { fontSize: 12, paddingLeft: 22 },
  toolOutput: { fontFamily: MONO, fontSize: 12, lineHeight: 17, paddingLeft: 22 },
  bar: { borderLeftWidth: 3, paddingLeft: 10, gap: 4 },
  mailHead: { fontSize: 13 },
  mailWho: { color: colors.sapphire, fontWeight: '700' },
  mailSubject: { color: colors.text, fontWeight: '700' },
  mailAt: { color: colors.overlay0 },
  event: { flexDirection: 'row', alignItems: 'center', gap: 8 },
  eventRule: { width: 24, height: 1, backgroundColor: colors.surface1 },
  eventText: { flex: 1, fontSize: 12 },
  chip: { position: 'absolute', alignSelf: 'center', bottom: 12, backgroundColor: colors.accent, borderRadius: 16, paddingHorizontal: 14, paddingVertical: 7 },
  chipText: { color: colors.crust, fontWeight: '800' },
});

import { useMemo, useState } from 'react';
import { Alert, Linking, Pressable, ScrollView, useWindowDimensions, View } from 'react-native';
import { useNavigation } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import type { TimelineEntry } from '../../../clients/typescript/st3-client';
import { ANSWERS, alertsIn, cleanMessageText, isRequest, offeredAnswers, pendingCall, promptAnswers, report, spaced, yesNo, ago, type KeptAttention, type PendingCall } from '@smalltalk/st3-views';
import { agentName } from '../agentsView';
import type { RootParams } from '../navigation';
import { attentionKindLabel } from '../presentation';
import { useStore } from '../store';
import { theme } from '../theme';
import { Button, Markdown, Note, T } from '../ui';

// Alerts in an agent's conversation: everything that waits on the person and belongs to this
// agent, above the conversation, answerable there. Each is a line that opens to its question and
// its answers; with one alert it starts open. An answer is the same action Home sends.

/** The alerts of one agent's conversation. Nothing at all when it has none. */
export function AlertsStrip({ agentId, entries }: { agentId: string; entries: ReadonlyArray<TimelineEntry> }) {
  const { data, caps } = useStore();
  const { height } = useWindowDimensions();
  const alerts = useMemo(() => alertsIn(data.attention, agentId, caps?.session_actor), [data.attention, agentId, caps?.session_actor]);
  const [toggled, setToggled] = useState<ReadonlySet<string>>(new Set());
  const call = useMemo(() => pendingCall(entries), [entries]);
  if (!alerts.length) return null;
  // One alert starts open; with several, each opens on a tap.
  const isOpen = (id: string) => (alerts.length === 1) !== toggled.has(id);
  const toggle = (id: string) => setToggled(previous => { const next = new Set(previous); if (next.has(id)) next.delete(id); else next.add(id); return next; });
  return <ScrollView style={{ maxHeight: Math.round(height * 0.4), flexGrow: 0, backgroundColor: theme.mantle }} contentContainerStyle={{ padding: 8, gap: 6 }}>
    {alerts.map(item => <AlertCard key={item.id} item={item} agentId={agentId} open={isOpen(item.id)} onToggle={() => toggle(item.id)} call={call} />)}
  </ScrollView>;
}

function AlertCard({ item, agentId, open, onToggle, call }: { item: KeptAttention; agentId: string; open: boolean; onToggle: () => void; call: PendingCall | null }) {
  const { data } = useStore();
  const navigation = useNavigation<NativeStackNavigationProp<RootParams>>();
  const seats = (item.conversation_ids ?? []).filter(id => id !== agentId).map(id => { const seat = data.agents.find(candidate => candidate.id === id); return seat ? agentName(seat) : id.replace(/^agent\//, ''); });
  const from = (() => { const asker = data.agents.find(candidate => candidate.id === (item.requester_id ?? agentId)); return asker ? agentName(asker) : (item.requester_id ?? agentId).replace(/^agent\//, ''); })();
  return <View style={{ borderLeftWidth: 3, borderLeftColor: theme.person, backgroundColor: theme.surface0, borderRadius: 6, padding: 8, gap: 6 }}>
    <Pressable accessibilityRole="button" accessibilityState={{ expanded: open }} onPress={onToggle}>
      <T numberOfLines={open ? undefined : 1}><T bold color={theme.person}>◆ alert  </T><T bold>{item.title}</T></T>
      {open ? null : <T dim numberOfLines={1}>{attentionKindLabel(item.attention_kind)} · waited {ago(item.requested_at)}</T>}
    </Pressable>
    {open ? <View style={{ gap: 8 }}>
      <T dim>{attentionKindLabel(item.attention_kind)} · waited {ago(item.requested_at)}</T>
      <AlertBody item={item} from={from} call={call} seats={seats} />
      <Button label="details" color={theme.subtext0} onPress={() => navigation.navigate('Attention', { id: item.id })} />
    </View> : null}
  </View>;
}

export function AlertBody({ item, from, call, seats }: { item: KeptAttention; from: string; call: PendingCall | null; seats: string[] }) {
  const prompt = promptAnswers(item);
  if (prompt) return <PromptAnswerView item={item} call={call} />;
  if (item.attention_kind === 'harness-prompt') return <View style={{ gap: 6 }}>
    {item.detail ? <Markdown text={spaced(cleanMessageText(item.detail))} color={theme.text} /> : null}
    <Note>This prompt is answered in the agent's terminal.</Note>
  </View>;
  if (item.attention_kind === 'harness-login') return <View style={{ gap: 6 }}>
    {item.detail ? <Markdown text={spaced(cleanMessageText(item.detail))} color={theme.text} /> : null}
    {seats.length ? <T dim>One sign-in clears it for {seats.join(', ')} too.</T> : null}
  </View>;
  if (isRequest(item.attention_kind) && item.actions.includes('work.done')) {
    return item.request
      ? <StructuredRequestView item={item} request={item.request} from={from} onAnswered={() => {}} />
      : <View style={{ gap: 8 }}><RequestQuestion text={cleanMessageText(item.detail ?? '')} /><RequestAnswers item={item} from={from} onAnswered={() => {}} /></View>;
  }
  return <View style={{ gap: 6 }}>
    {item.detail ? <Markdown text={spaced(cleanMessageText(item.detail))} color={theme.subtext0} /> : null}
    {item.because ? <T soft>because {item.because}</T> : null}
  </View>;
}

// A Claude permission prompt: the call it would allow, in full, beside the answers. Allow is
// offered only when the call can be shown; deny always.
function PromptAnswerView({ item, call }: { item: KeptAttention; call: PendingCall | null }) {
  const { busy, status, actions } = useStore();
  const prompt = promptAnswers(item)!;
  const disabled = busy || status !== 'online';
  const offered = offeredAnswers(prompt, call);
  const send = (answer: 'allow' | 'deny') => {
    const shown = call ? `${call.tool}${call.lines[0] ? `\n${call.lines[0]}` : ''}` : undefined;
    Alert.alert(answer === 'allow' ? 'Allow this call?' : 'Deny this call?', shown, [
      { text: 'Cancel', style: 'cancel' },
      { text: answer === 'allow' ? 'Allow' : 'Deny', style: answer === 'deny' ? 'destructive' : 'default', onPress: () => void actions.respondPrompt(item, answer) },
    ]);
  };
  return <View style={{ gap: 8 }}>
    {item.detail ? <T selectable>{cleanMessageText(item.detail)}</T> : null}
    {call && call.lines.length
      ? <View style={{ backgroundColor: theme.crust, borderRadius: 4, padding: 8, gap: 2 }}>
          <T bold color={theme.person}>{call.tool}</T>
          {call.lines.map((line, index) => <T key={index} selectable>{line}</T>)}
        </View>
      : <Note tone="warning">The call it would allow is not in the conversation yet, so only deny is offered. Allow it in the terminal.</Note>}
    <View style={{ flexDirection: 'row', gap: 8, flexWrap: 'wrap' }}>
      {offered.includes('allow') ? <Button label="Allow" color={theme.green} disabled={disabled} onPress={() => send('allow')} /> : null}
      {offered.includes('deny') ? <Button label="Deny" color={theme.red} disabled={disabled} onPress={() => send('deny')} /> : null}
    </View>
    <Note>Only this prompt, once. An answer in the terminal wins.</Note>
  </View>;
}

// A structured request (#1010) as stui shows it: the recommendation first, then the question, its
// summary, reasons and links, and each named answer with what it does. A tap on an answer asks
// to confirm, then sends it by its id.
type Structured = NonNullable<Parameters<ReturnType<typeof useStore>['actions']['done']>[0]['request']>;
export function StructuredRequestView({ item, request, from, onAnswered }: { item: Parameters<ReturnType<typeof useStore>['actions']['done']>[0]; request: Structured; from: string; onAnswered: () => void }) {
  const { busy, status, actions } = useStore();
  const disabled = busy || status !== 'online';
  const label = (id: string) => request.answers?.find(answer => answer.id === id)?.label ?? id;
  const send = (answer: { id: string; label: string; outcome?: string | null }) => answer.outcome === 'request_changes'
    // Requesting changes needs the changes, in words.
    ? Alert.prompt(answer.label, 'What should change?', text => { if (text.trim()) void actions.done(item, text.trim(), answer.id).then(done => { if (done) onAnswered(); }); })
    : Alert.alert(`Answer ${from} “${answer.label}”`, undefined, [
    { text: 'Cancel', style: 'cancel' },
    { text: 'Send', onPress: () => void actions.done(item, answer.label, answer.id).then(done => { if (done) onAnswered(); }) },
  ]);
  const words = () => Alert.prompt(`Answer ${from}`, item.title, text => { if (text.trim()) void actions.done(item, text.trim()).then(done => { if (done) onAnswered(); }); });
  return <View style={{ gap: 8 }}>
    {request.recommendation ? <T><T dim>recommends  </T><T bold color={theme.green}>{label(request.recommendation.answer)}</T><T soft>  {request.recommendation.reason}</T></T> : null}
    <RequestQuestion text={request.question} />
    {request.summary ? <Markdown text={spaced(request.summary)} color={theme.subtext0} /> : null}
    {request.reasons?.length ? <View style={{ gap: 2 }}>{request.reasons.map((reason, index) => <T key={index}><T color={theme.lavender}>•  </T>{reason}</T>)}</View> : null}
    {request.subjects?.map((subject, index) => subject.url
      ? <Pressable key={index} onPress={() => void Linking.openURL(subject.url!)}><T color={theme.accent}>↗ {subject.label}</T></Pressable>
      : <T key={index} dim>↗ {subject.label}  {subject.ref ?? ''}</T>)}
    <T bold color={theme.person}>{from} is waiting on you.</T>
    {request.answers?.map(answer => <Pressable key={answer.id} disabled={disabled} onPress={() => send(answer)}
      style={{ borderLeftWidth: 2, borderLeftColor: request.recommendation?.answer === answer.id ? theme.green : theme.surface1, paddingLeft: 8, paddingVertical: 4, opacity: disabled ? 0.5 : 1 }}>
      <T bold color={theme.accent}>{answer.label}{request.recommendation?.answer === answer.id ? <T color={theme.green}>  recommended</T> : null}{answer.outcome === 'request_changes' ? <T color={theme.yellow}>  needs your words</T> : null}</T>
      <T dim>{answer.consequence}</T>
    </Pressable>)}
    {request.custom ? <Button label="Answer in words" disabled={disabled} onPress={words} /> : null}
    {request.why_person ? <T dim>Why you: {request.why_person}</T> : null}
  </View>;
}

// A request's question: a JSON report as its telling fields, anything else as Markdown.
export function RequestQuestion({ text }: { text: string }) {
  const shown = report(text);
  if (!shown) return <Markdown text={spaced(text)} color={theme.text} />;
  const tone = { fault: theme.red, text: theme.text, soft: theme.subtext0 } as const;
  return <View style={{ gap: 2 }}>
    {shown.before ? <Markdown text={shown.before} color={theme.text} /> : null}
    {shown.rows.map(row => <T key={row.key} numberOfLines={1}><T dim>{row.key}  </T><T color={tone[row.tone]}>{row.value}</T></T>)}
    {shown.more ? <T dim>… {shown.more} more fields</T> : null}
  </View>;
}

// stui's answers: Yes and No for a yes-or-no question, words, or Nothing to do. Each completes
// the waiting step with a short reason, after the person confirms it.
export function RequestAnswers({ item, from, onAnswered }: { item: Parameters<ReturnType<typeof useStore>['actions']['done']>[0]; from: string; onAnswered: () => void }) {
  const { busy, status, actions } = useStore();
  const disabled = busy || status !== 'online';
  const send = (reason: string) => void actions.done(item, reason).then(done => { if (done) onAnswered(); });
  const confirm = (label: string, reason: string) => Alert.alert(label, undefined, [{ text: 'Cancel', style: 'cancel' }, { text: 'Send', onPress: () => send(reason) }]);
  const words = () => Alert.prompt(`Answer ${from}`, item.title, text => { if (text.trim()) send(text.trim()); });
  return <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 8 }}>
    {yesNo(item.title) ? <>
      <Button label="Yes" color={theme.green} disabled={disabled} onPress={() => confirm(`Answer ${from} “Yes”`, ANSWERS.yes)} />
      <Button label="No" color={theme.red} disabled={disabled} onPress={() => confirm(`Answer ${from} “No”`, ANSWERS.no)} />
      <Button label="Answer in words" disabled={disabled} onPress={words} />
    </> : <Button label="Answer" disabled={disabled} onPress={words} />}
    <Button label="Nothing to do" color={theme.overlay1} disabled={disabled} onPress={() => confirm(`Tell ${from} there is nothing for you to do`, ANSWERS.nothing)} />
  </View>;
}

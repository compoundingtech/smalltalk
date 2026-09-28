// One card per kind of attention. Each shows the thing being decided (the question, the draft,
// the proposed mission, the plan change, the fault, the message), what it relates to, "Chat
// about this", and "Go to". Binding actions take a second tap.

import { useEffect, useState } from 'react';
import { StyleSheet, Text, View } from 'react-native';
import type { Attention, World } from './clientView';
import { harnessName, items } from './clientView';
import { EntryList } from './ConversationView';
import { useLinks } from './navigation';
import { aboutThread, chatTarget, flowLayers, goToSubject } from './screenModel';
import { useStore, type CardAction } from './store';
import { colors, type ColorToken } from './theme';
import { Body, Button, Buttons, Card, Composer, Dim, Field, Label, Markdown, MONO, TextArea } from './ui';
import { attentionStyle, harnessColor } from './words';

function Link({ prefix, label, subject }: { prefix: string; label: string; subject: string }) {
  const { peek } = useLinks();
  return (
    <Text style={styles.linkLine}>
      <Text style={styles.linkPrefix}>{prefix}</Text>
      <Text style={styles.link} onPress={() => peek(subject)} accessibilityRole="link">{label}</Text>
    </Text>
  );
}

export const missionName = (world: World, id: string) => items(world.missions).find(mission => mission.id === id)?.title ?? id.replace(/^mission\//, '');
export const agentName = (world: World, id: string) => items(world.agents).find(agent => agent.id === id)?.name ?? id.replace(/^agent\//, '');

/** Steps as a flow: `scan → fix → {review, lint} → merge`, coloured by who acts. */
export function Flow({ steps }: { steps: { name: string; after: string[]; color: ColorToken }[] }) {
  const layers = flowLayers(steps);
  return (
    <Text style={styles.flow}>
      {layers.map((layer, index) => (
        <Text key={index}>
          {index > 0 ? <Text style={{ color: colors.surface2 }}> → </Text> : null}
          {layer.length > 1 ? <Text style={styles.flowDim}>{'{'}</Text> : null}
          {layer.map((step, position) => (
            <Text key={step.name}>{position > 0 ? <Text style={styles.flowDim}>, </Text> : null}<Text style={{ color: colors[step.color] }}>{step.name}</Text></Text>
          ))}
          {layer.length > 1 ? <Text style={styles.flowDim}>{'}'}</Text> : null}
        </Text>
      ))}
    </Text>
  );
}

function TextBox({ title, value, onChange, placeholder, autoFocus }: { title: string; value: string; onChange: (text: string) => void; placeholder?: string; autoFocus?: boolean }) {
  return (
    <View style={styles.box}>
      <Label color="accent">{title}</Label>
      <TextArea value={value} onChange={onChange} placeholder={placeholder ?? 'Write here'} autoFocus={autoFocus} />
    </View>
  );
}

export function AttentionCard({ item }: { item: Attention }) {
  const store = useStore();
  const { world } = store;
  const { goTo } = useLinks();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');
  const [chat, setChat] = useState(false);
  const [chatDraft, setChatDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const target = chatTarget(world, item);
  const { glyph, color } = attentionStyle(item.kind.kind);
  const heavy = color === 'person';

  useEffect(() => store.watchAttention(item.id), [store.watchAttention, item.id]);
  useEffect(() => (chat && target ? store.watchConversation(target.id) : undefined), [chat, target?.id, store.watchConversation]);

  const act = async (action: CardAction, text?: string) => {
    setBusy(true);
    const ok = await store.act(item, action, text);
    setBusy(false);
    if (ok) { setDraft(''); setEditing(false); }
  };
  const sendChat = async () => {
    if (!target || !chatDraft.trim()) return;
    setBusy(true);
    const ok = await store.discuss(item, target.id, chatDraft.trim());
    setBusy(false);
    if (ok) setChatDraft('');
  };
  const writing = editing || draft.length > 0;
  const textActions = (title: string, send: string, action: CardAction) => (
    <>
      <TextBox title={title} value={draft} onChange={setDraft} autoFocus />
      <Buttons>
        <Button label={send} color="yellow" filled disabled={busy || !draft.trim()} onPress={() => void act(action, draft)} />
        <Button label="Cancel" color="overlay1" onPress={() => { setEditing(false); setDraft(''); }} />
      </Buttons>
    </>
  );

  let body: React.ReactNode;
  switch (item.kind.kind) {
    case 'review': {
      const kind = item.kind;
      body = (
        <>
          <Markdown text={kind.question} />
          <Field label="because">{kind.because}</Field>
          {kind.look_at.map(([label, value], index) => <Field key={`${label}${index}`} label={label} color="text">{value}</Field>)}
          {kind.step ? <Field label="step">{kind.step}</Field> : null}
          {writing ? textActions('what should change · the agent reads this', 'Send back with these notes', 'send-back') : (
            <Buttons>
              <Button label="Approve" color="green" confirm="Approve" disabled={busy} onPress={() => void act('approve')} />
              <Button label="Request changes" color="yellow" onPress={() => setEditing(true)} />
            </Buttons>
          )}
        </>
      );
      break;
    }
    case 'feedback': {
      const kind = item.kind;
      body = (
        <>
          <Markdown text={kind.question} />
          <Card title={kind.subject} style={styles.inner}><Markdown text={kind.excerpt.join('\n')} color="subtext0" /></Card>
          {kind.link ? <Text style={styles.linkLine}><Text style={styles.linkPrefix}>open  </Text><Text style={styles.link}>{kind.link}</Text></Text> : null}
          <TextBox title="your feedback · the agent reads this" value={draft} onChange={setDraft} placeholder="Write feedback" />
          <Buttons>
            <Button label="Send feedback" color="accent" filled disabled={busy || !draft.trim()} onPress={() => void act('feedback', draft)} />
            <Button label="Looks good as is" color="green" confirm="Say it looks good" disabled={busy} onPress={() => void act('approve')} />
          </Buttons>
        </>
      );
      break;
    }
    case 'launch': {
      const kind = item.kind;
      const preview = kind.preview;
      const asks = preview.state === 'ready' ? preview.value.steps.filter(step => step.asks_you).length : 0;
      body = (
        <>
          <Body>{kind.planner} proposes a new mission. Nothing runs until you approve it.</Body>
          <Field label="mission" color="text">{preview.state === 'ready' ? preview.value.name : kind.name}</Field>
          {preview.state === 'loading' ? <Dim>Loading the proposed mission…</Dim> : null}
          {preview.state === 'failed' ? <Card title="nothing to approve yet" color="yellow" heavy style={styles.inner}><Markdown text={preview.value} /></Card> : null}
          {preview.state === 'ready' ? (
            <>
              {preview.value.workspace ? <Field label="in">{preview.value.workspace}</Field> : null}
              <Field label="asks you" color="person">{`${asks} time${asks === 1 ? '' : 's'} before it finishes`}</Field>
              <Label>goals</Label>
              {preview.value.goals.map(goal => <View key={goal} style={styles.goal}><Text style={styles.goalMark}>◆</Text><Markdown text={goal} style={styles.flex} /></View>)}
              <Label>{`steps · ${preview.value.steps.length}`}</Label>
              <Flow steps={preview.value.steps.map(step => ({ name: step.name, after: step.after, color: step.asks_you ? 'person' : 'subtext1' }))} />
              {preview.value.steps.map(step => (
                <View key={step.name} style={styles.stepRow}>
                  <Text style={styles.stepName}>{step.name}</Text>
                  <Text style={[styles.stepWho, { color: step.asks_you ? colors.person : colors.subtext0 }]}>{step.assignee}</Text>
                  <Dim>{step.after.length ? `after ${step.after.join(', ')}` : 'first'}</Dim>
                </View>
              ))}
              <Label>{`agents · ${preview.value.agents.length}`}</Label>
              {preview.value.agents.map(agent => (
                <View key={agent.name} style={styles.stepRow}>
                  <Text style={styles.stepName}>{agent.name}</Text>
                  <Text style={[styles.stepWho, { color: colors[harnessColor(agent.harness)] }]}>{harnessName(agent.harness)}</Text>
                  <Dim>{agent.host ? `on ${agent.host}` : 'host not said'}</Dim>
                </View>
              ))}
            </>
          ) : null}
          {writing ? textActions('what the planner should change', 'Send to the planner', 'ask-changes') : (
            <Buttons>
              {preview.state === 'ready' ? <Button label="Approve launch" color="green" confirm="Approve and start this mission" disabled={busy} onPress={() => void act('approve')} /> : null}
              <Button label={preview.state === 'ready' ? 'Ask for changes' : 'Ask the planner'} color="yellow" onPress={() => setEditing(true)} />
              <Button label="Cancel launch" color="red" confirm="Cancel this launch" disabled={busy} onPress={() => void act('cancel')} />
            </Buttons>
          )}
        </>
      );
      break;
    }
    case 'revision': {
      const kind = item.kind;
      body = (
        <>
          <Markdown text={kind.reason} />
          <Label>{`changes to the plan · ${kind.changes.length}`}</Label>
          {kind.changes.length === 0 ? <Dim>st does not say what changes yet.</Dim> : null}
          {kind.changes.map(([sign, change], index) => {
            const tone = sign === '+' ? colors.green : sign === '-' ? colors.red : colors.yellow;
            return <Text key={index} selectable style={[styles.change, { color: tone }]}><Text style={styles.bold}>{sign} </Text>{change}</Text>;
          })}
          {writing ? textActions('what should change · the proposing agent reads this', 'Ask for these changes', 'ask-changes') : (
            <Buttons>
              <Button label="Approve revision" color="green" confirm="Approve the revision" disabled={busy} onPress={() => void act('approve')} />
              <Button label="Ask for changes" color="yellow" onPress={() => setEditing(true)} />
              <Button label="Reject" color="red" confirm="Reject the revision" disabled={busy} onPress={() => void act('reject')} />
            </Buttons>
          )}
        </>
      );
      break;
    }
    case 'fault': {
      const kind = item.kind;
      body = (
        <>
          <Markdown text={kind.what} />
          <Field label="because">{kind.because}</Field>
          {kind.source ? <Field label="source">{kind.source}</Field> : null}
          {kind.fix ? <Card title="suggested fix" color="green" heavy style={styles.inner}><Markdown text={kind.fix} /></Card> : null}
          <Buttons><Button label="Mark resolved" color="green" confirm="Mark this fault resolved" disabled={busy} onPress={() => void act('resolve')} /></Buttons>
        </>
      );
      break;
    }
    case 'message': {
      const kind = item.kind;
      body = (
        <>
          <Text style={styles.linkLine}><Text style={styles.linkPrefix}>from  </Text><Text style={styles.from}>{kind.from}</Text></Text>
          <Markdown text={kind.body} />
          {writing ? textActions(`reply to ${kind.from}`, 'Send reply', 'reply') : (
            <Buttons>
              <Button label="Reply" color="accent" onPress={() => setEditing(true)} />
              <Button label="Mark read" color="green" disabled={busy} onPress={() => void act('read')} />
              <Button label={store.mode === 'demo' ? 'Remind me later · demo' : 'Remind me later · this device'} color="overlay1" onPress={() => store.snooze(item.id)} />
            </Buttons>
          )}
        </>
      );
      break;
    }
  }

  const subject = goToSubject(item);
  const thread = chat && target ? aboutThread(world, target.id, item) : [];
  return (
    <Card title={`${item.kind.kind} · waiting ${item.age}`} color={heavy ? 'person' : color} heavy={heavy}>
      <Markdown text={`**${item.title}**`} />
      <Text style={styles.meta}>
        <Text style={{ color: colors[color] }}>{`${glyph}\uFE0E ${item.kind.kind} `}</Text>
        <Text style={styles.metaDim}>{item.waiting ? `· ${item.waiting} is waiting · ` : '· '}{item.age} ago</Text>
      </Text>
      {item.mission ? <Link prefix="mission  " label={missionName(world, item.mission)} subject={item.mission} /> : null}
      {item.agent ? <Link prefix="agent    " label={agentName(world, item.agent)} subject={item.agent} /> : null}
      <View style={styles.gap} />
      {body}
      <View style={styles.gap} />
      <Label>related</Label>
      <Field label="raised by" color={item.raised_by ? 'subtext0' : 'overlay0'}>{item.raised_by ?? 'st does not say yet'}</Field>
      {item.related.length === 0 && !item.mission && !item.agent ? <Dim>st names no agent, mission or step for this item.</Dim> : null}
      {item.related.map(([related, state]) => (
        /^(agent|mission|attention|session)\//.test(related)
          ? <View key={related}><Link prefix={`${related.split('/')[0].padEnd(10)}`} label={related} subject={related} />{state ? <Dim style={styles.relatedState}>{state}</Dim> : null}</View>
          : <Field key={related} label={related.split('/')[0]}>{state ? `${related} · ${state}` : related}</Field>
      ))}
      <View style={styles.gap} />
      {chat && target ? (
        <>
          <Label color="sapphire">{`chat with ${target.name} about this`}</Label>
          {thread.length ? <EntryList entries={thread} /> : <Dim>{`${target.name} gets this item as context: its title, mission and question.`}</Dim>}
          <Composer value={chatDraft} onChange={setChatDraft} placeholder={`Message ${target.name}`} onSend={() => void sendChat()} disabled={busy} autoFocus />
          <Buttons><Button label="Close chat" color="overlay1" onPress={() => setChat(false)} /></Buttons>
        </>
      ) : (
        <Buttons>
          <Button label="Chat about this" color="sapphire" onPress={() => { if (target) setChat(true); else store.clearNotice(); }} disabled={!target} />
          {subject ? <Button label={item.mission ? 'Go to the mission' : 'Go to the agent'} color="overlay1" onPress={() => goTo(subject)} /> : null}
        </Buttons>
      )}
    </Card>
  );
}

const styles = StyleSheet.create({
  flex: { flex: 1 },
  gap: { height: 4 },
  inner: { backgroundColor: colors.crust },
  meta: { fontSize: 13 },
  metaDim: { color: colors.overlay0 },
  linkLine: { fontSize: 14, lineHeight: 20 },
  linkPrefix: { color: colors.overlay0, fontFamily: MONO, fontSize: 13 },
  link: { color: colors.blue, textDecorationLine: 'underline' },
  from: { color: colors.sapphire, fontWeight: '700' },
  goal: { flexDirection: 'row', gap: 6 },
  goalMark: { color: colors.lavender, lineHeight: 21 },
  flow: { fontSize: 14, lineHeight: 20 },
  flowDim: { color: colors.overlay0 },
  stepRow: { flexDirection: 'row', gap: 8, alignItems: 'baseline' },
  stepName: { color: colors.text, width: 96, fontSize: 14 },
  stepWho: { width: 120, fontSize: 14 },
  change: { fontFamily: MONO, fontSize: 13, lineHeight: 18 },
  bold: { fontWeight: '800' },
  box: { gap: 6 },
  relatedState: { paddingLeft: 88 },
});

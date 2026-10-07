import { Alert, Pressable, View } from 'react-native';
import { useStore } from './store';
import { theme } from './theme';
import { Markdown, T } from './ui';

type Item = Parameters<ReturnType<typeof useStore>['actions']['respondPrompt']>[0];

// A prompt a harness is waiting on: what it asks and its choices while it can be answered here;
// once it ended, or where nothing can be answered, how it ended and what to do instead, never a
// button. The same card sits on Home and above a conversation's message box.
export function PromptCard({ item, from, onAnswered }: { item: Item; from: string; onAnswered?: () => void }) {
  const { busy, status, actions } = useStore();
  const prompt = item.prompt;
  if (!prompt) return null;
  const open = !prompt.state || prompt.state === 'open';
  const answerable = open && prompt.can_answer !== false && item.actions.includes('prompt.respond');
  const disabled = busy || status !== 'online';
  const send = (choice: { id: string; label: string }) => Alert.alert(`Answer ${from} “${choice.label}”`, undefined, [
    { text: 'Cancel', style: 'cancel' },
    { text: 'Send', onPress: () => void actions.respondPrompt(item, choice.id).then(done => { if (done) onAnswered?.(); }) },
  ]);
  return <View style={{ gap: 8 }}>
    <Markdown text={prompt.content || item.detail || item.title} color={theme.text} />
    {answerable ? <T bold color={theme.person}>{from} is waiting on you.</T> : null}
    {answerable ? prompt.choices?.map(choice => <Pressable key={choice.id} disabled={disabled} onPress={() => send(choice)}
      style={{ borderLeftWidth: 2, borderLeftColor: theme.surface1, paddingLeft: 8, paddingVertical: 4, opacity: disabled ? 0.5 : 1 }}>
      <T bold color={theme.accent}>{choice.label}</T>
      <T dim>{choice.consequence}</T>
    </Pressable>) : null}
    {prompt.next_action ? <T color={theme.yellow}>{prompt.next_action}</T> : null}
    {!open ? <T dim>This prompt {prompt.state?.replace(/_/g, ' ')}{prompt.how ? `: ${prompt.how}` : ''}.</T> : null}
  </View>;
}

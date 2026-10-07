import SegmentedControl from '@react-native-segmented-control/segmented-control';
import { useEffect, useLayoutEffect, useState } from 'react';
import { ScrollView } from 'react-native';
import { Banners, useListsOnFocus } from '../chrome';
import { agentBranch, agentParameters, EFFORTS, HARNESSES, models, randomName, type Harness } from '../launcher';
import type { RootScreen } from '../navigation';
import { errorText, useStore } from '../store';
import { Button, Field, Screen, SectionHeader, T } from '../ui';

// A new agent, as stui's launcher starts one: what it should do (its first message), a name
// nobody has to think of, and how it runs. It opens in its conversation once st has it.
export function NewAgentScreen({ navigation }: RootScreen<'NewAgent'>) {
  const { busy, data, gatewayHost, client, status, actions } = useStore();
  useListsOnFocus(['machines']);
  const [message, setMessage] = useState(''), [name, setName] = useState(randomName);
  const [harness, setHarness] = useState<Harness>('claude'), [model, setModel] = useState('default'), [effort, setEffort] = useState('default');
  // This machine first (the gateway's own host), then the fleet's others.
  const hosts = ['this machine', ...data.machines.map(machine => machine.name).filter(host => host !== gatewayHost)];
  const [host, setHost] = useState(0);
  const selectedHost = host ? data.machines.find(machine => machine.name === hosts[host])?.id.replace(/^machine\//, 'host/') : undefined;
  const hostId = selectedHost ?? 'local';
  const [repository, setRepository] = useState(''), [branch, setBranch] = useState(''), [base, setBase] = useState('origin/main'), [workspace, setWorkspace] = useState('');
  const [suggestions, setSuggestions] = useState<{ host: string; paths: string[]; error?: string } | null>(null);
  useEffect(() => {
    let active = true;
    setSuggestions(null);
    if (!client || status !== 'online') return;
    void client.hostRepositories(hostId).then(reply => {
      if (active) setSuggestions({ host: hostId, paths: reply.value.repositories.map(repo => repo.path) });
    }).catch(error => { if (active) setSuggestions({ host: hostId, paths: [], error: errorText(error) }); });
    return () => { active = false; };
  }, [client, hostId, status]);
  const currentSuggestions = suggestions?.host === hostId ? suggestions : null;
  useLayoutEffect(() => {
    navigation.setOptions({ unstable_headerLeftItems: () => [{ type: 'button', label: 'Cancel', onPress: () => navigation.goBack() }] });
  }, [navigation]);
  const offered = models(harness);
  const start = () => void actions.createAgent(agentParameters({ message, name, harness, model, effort, host: selectedHost, repository, branch, base, workspace }))
    .then(agentId => { if (agentId) navigation.replace('Conversation', { target: agentId, title: name.trim() }); });
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, paddingBottom: 48 }} keyboardShouldPersistTaps="handled" keyboardDismissMode="interactive">
      <T soft>What should it do? That is its first message. It starts on the machine you pick and opens in its conversation.</T>
      <Field placeholder="what should it do?" multiline value={message} onChangeText={setMessage} />
      <Field placeholder="name" autoCapitalize="none" autoCorrect={false} spellCheck={false} value={name} onChangeText={setName} />
      <SectionHeader title="harness" />
      <SegmentedControl values={[...HARNESSES]} selectedIndex={HARNESSES.indexOf(harness)} appearance="dark" style={{ marginTop: 4 }}
        onChange={event => { setHarness(HARNESSES[event.nativeEvent.selectedSegmentIndex]); setModel('default'); }} />
      {offered.length > 1 ? <>
        <SectionHeader title="model" />
        <SegmentedControl values={offered.map(value => value.replace(/^claude-/, ''))} selectedIndex={Math.max(0, offered.indexOf(model))} appearance="dark" style={{ marginTop: 4 }}
          onChange={event => setModel(offered[event.nativeEvent.selectedSegmentIndex])} />
      </> : null}
      <SectionHeader title="effort" />
      <SegmentedControl values={[...EFFORTS]} selectedIndex={EFFORTS.indexOf(effort as typeof EFFORTS[number])} appearance="dark" style={{ marginTop: 4 }}
        onChange={event => setEffort(EFFORTS[event.nativeEvent.selectedSegmentIndex])} />
      {hosts.length > 1 ? <>
        <SectionHeader title="machine" />
        <SegmentedControl values={hosts} selectedIndex={host} appearance="dark" style={{ marginTop: 4 }} onChange={event => { setHost(event.nativeEvent.selectedSegmentIndex); setRepository(''); setWorkspace(''); }} />
      </> : null}
      <SectionHeader title="repository" />
      <T soft>Choose a repository for a worktree, or leave it empty for a plain workspace. Paths belong to the selected machine.</T>
      <Field accessibilityLabel="Repository" placeholder="repository path (optional)" autoCapitalize="none" autoCorrect={false} spellCheck={false} value={repository} onChangeText={setRepository} />
      {currentSuggestions ? currentSuggestions.error ? <T dim>Suggestions unavailable: {currentSuggestions.error}</T> : currentSuggestions.paths.map(path => <Button key={path} label={path} onPress={() => setRepository(path)} />) : <T dim>{status === 'online' ? 'Loading this machine’s repositories…' : 'Connect to load repositories, or type a path.'}</T>}
      {repository.trim() ? <>
        <SectionHeader title="branch" />
        <Field accessibilityLabel="Branch" placeholder={agentBranch(name.trim())} autoCapitalize="none" autoCorrect={false} spellCheck={false} value={branch} onChangeText={setBranch} />
        <SectionHeader title="base" />
        <Field accessibilityLabel="Base" placeholder="origin/main" autoCapitalize="none" autoCorrect={false} spellCheck={false} value={base} onChangeText={setBase} />
      </> : null}
      <SectionHeader title="workspace" />
      <Field accessibilityLabel="Workspace" placeholder="empty: st chooses a new directory" autoCapitalize="none" autoCorrect={false} spellCheck={false} value={workspace} onChangeText={setWorkspace} />
      <Button label="start" disabled={busy || !name.trim()} onPress={start} />
    </ScrollView>
  </Screen>;
}

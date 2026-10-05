import { useFocusEffect, useNavigation, useRoute, type RouteProp } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import { useCallback, useMemo, useRef, useState } from 'react';
import { RefreshControl, ScrollView, View } from 'react-native';
import type { UsagePeriod } from '../../../clients/typescript/st3-client';
import { plainError } from '../../../clients/typescript/st3-client';
import { agentName } from '../agentsView';
import { Banners, StatusLine } from '../chrome';
import type { StackParams } from '../navigation';
import { useStore } from '../store';
import { theme } from '../theme';
import { accounts, byOf, cost, groups, label, limitLine, mix, nextBy, PERIODS, periodName, tokenShare, tokens, totalOf, type By, type Names, type Tone } from '../usage';
import { Button, ListRow, Note, Screen, SectionHeader, T } from '../ui';

// Usage: spend and its API-equivalent cost over a period, from st's usage read, as stui's Usage
// tab shows it. st keeps no stream of usage, so it is read on focus, again each minute while the
// tab is shown, and at once over a new period.
const READ_EVERY_MS = 60_000;
const tone: Record<Tone, string> = { ok: theme.idle, warning: theme.waiting, fault: theme.fault, quiet: theme.overlay0 };
// The last read, so a detail screen shows what the list showed.
let last: { hours: number; period: UsagePeriod } | null = null;

export function useUsage(hours: number) {
  const { client } = useStore();
  const [period, setPeriod] = useState<UsagePeriod | null>(last?.hours === hours ? last.period : null);
  const [issue, setIssue] = useState('');
  const [reading, setReading] = useState(false);
  const read = useCallback(async () => {
    if (!client) return;
    setReading(true);
    try {
      const result = await client.usagePeriod({ since_ms: Date.now() - hours * 3_600_000 });
      last = { hours, period: result.value };
      setPeriod(result.value);
      setIssue('');
    } catch (error) {
      const text = plainError(error);
      // A daemon from before the read answers its path with a bare 404.
      setIssue(/404|not-found|it is gone/i.test(text) ? 'This st does not serve usage yet: its daemon needs an update.' : `Could not read usage: ${text}`);
    } finally {
      setReading(false);
    }
  }, [client, hours]);
  useFocusEffect(useCallback(() => {
    void read();
    const timer = setInterval(() => void read(), READ_EVERY_MS);
    return () => clearInterval(timer);
  }, [read]));
  return { period, issue, reading, read };
}

export function useNames(): Names {
  const { data } = useStore();
  return useMemo(() => ({
    agents: new Map(data.agents.map(agent => [agent.id, agentName(agent)])),
    missions: new Map(data.missions.map(mission => [mission.id, mission.title])),
  }), [data.agents, data.missions]);
}

export function UsageScreen() {
  const navigation = useNavigation<NativeStackNavigationProp<StackParams>>();
  const [hours, setHours] = useState<number>(PERIODS[0]);
  const [by, setBy] = useState<By>('agent');
  const { period, issue, reading, read } = useUsage(hours);
  const names = useNames();
  const scroll = useRef<ScrollView>(null);
  const now = Date.now();
  const rows = period?.rows ?? [];
  const limits = period?.limits ?? [];
  const total = totalOf(rows);
  const open = (id: string) => navigation.navigate('UsageDetail', { id, hours });
  const top = (grouping: By) => groups(rows, grouping).find(group => !group.id.startsWith('usage/'));
  return <Screen>
    <Banners />
    <ScrollView ref={scroll} contentInsetAdjustmentBehavior="automatic" refreshControl={<RefreshControl refreshing={reading} onRefresh={() => void read()} tintColor={theme.overlay1} />} contentContainerStyle={{ paddingBottom: 32 }}>
      <StatusLine />
      <View style={{ flexDirection: 'row', gap: 8, paddingHorizontal: 12, paddingTop: 8 }}>
        <Button label={periodName(hours)} onPress={() => setHours(PERIODS[(PERIODS.indexOf(hours as typeof PERIODS[number]) + 1) % PERIODS.length])} />
        <Button label={`by ${by}`} onPress={() => setBy(nextBy(by))} />
      </View>
      {issue ? <Note tone="warning">{issue}</Note> : null}
      {!period && !issue ? <Note>Loading usage…</Note> : null}
      {period && !rows.length && !limits.length ? <Note>No spend recorded in this period.</Note> : null}
      {rows.length || limits.length ? <>
        <SectionHeader title="accounts" count={accounts(rows, limits).length} />
        {accounts(rows, limits).map(({ id, total: spent, limit }) => {
          const line = limit ? limitLine(limit, now) : null;
          return <ListRow key={id} title={<T numberOfLines={1}><T bold>{label(id, names, rows)}</T>{spent.tokens ? <T dim>  {tokenShare(spent.cached, spent.tokens)} cached</T> : null}</T>} right={<T color={theme.accent}>{cost(spent)}</T>} onPress={() => open(id)}
            second={line
              ? <T dim>weekly <T bold color={tone[line.weekly.tone]}>{line.weekly.text}</T>{line.resets ? ` ${line.resets}` : ''} · 5-hour <T bold color={tone[line.fiveHour.tone]}>{line.fiveHour.text}</T> · {line.measured}</T>
              : `${tokens(spent.tokens)} tokens · ${tokenShare(spent.cached, spent.tokens)} cached · no limits reported`} />;
        })}
        <SectionHeader title="summary" />
        <T style={{ paddingHorizontal: 12, paddingVertical: 4 }}><T bold>{cost(total)}</T><T dim> · {tokens(total.tokens)} tokens · {periodName(hours)}</T></T>
        <T dim style={{ paddingHorizontal: 12 }}>{mix(total)}</T>
        {(['agent', 'mission'] as const).map(grouping => {
          const best = top(grouping);
          return best ? <T key={grouping} style={{ paddingHorizontal: 12, paddingVertical: 2 }}><T dim>most by {grouping}  </T>{label(best.id, names, rows)}<T color={theme.accent}>  {cost(best.total)}</T></T> : null;
        })}
        {total.unpriced > 0 ? <Note tone="warning">{tokens(total.unpriced)} tokens had no price, so the cost is at least this.</Note> : null}
        <SectionHeader title={`by ${by}`} count={groups(rows, by).length} />
        {groups(rows, by).map(({ id, total: spent }) => <ListRow key={id} title={label(id, names, rows)} right={<T color={theme.accent}>{cost(spent)}</T>} onPress={() => open(id)}
          second={`${tokens(spent.tokens)} tokens · ${tokenShare(spent.cached, spent.tokens)} cached · ${tokens(spent.output)} out`} />)}
        <Note>Costs are API-equivalent list prices.</Note>
      </> : null}
    </ScrollView>
  </Screen>;
}

// One group's spend: its total, how its tokens split, and where they went by step, agent, model
// and host; an account's (or the whole period's) limits too.
export function UsageDetailScreen() {
  const route = useRoute<RouteProp<StackParams, 'UsageDetail'>>();
  const { id, hours } = route.params;
  const { period, issue } = useUsage(hours);
  const names = useNames();
  const navigation = useNavigation<NativeStackNavigationProp<StackParams>>();
  const now = Date.now();
  const rows = period?.rows ?? [];
  const own = byOf(id);
  const mine = own ? rows.filter(row => groups([row], own)[0]?.id === id) : rows;
  const total = totalOf(mine);
  const limits = (period?.limits ?? []).filter(limit => id === `account/${limit.account}`);
  return <Screen>
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, paddingBottom: 32 }}>
      <T bold style={{ fontSize: 17 }}>{label(id, names, rows)}</T>
      {issue ? <Note tone="warning">{issue}</Note> : null}
      {!mine.length ? <Note>No spend recorded in {periodName(hours)}.</Note> : <>
        <T style={{ paddingVertical: 4 }}><T bold color={theme.accent}>{cost(total)}</T><T dim> API-equivalent · {tokens(total.tokens)} tokens · {tokenShare(total.cached, total.tokens)} cached · {periodName(hours)}</T></T>
        {total.unpriced > 0 ? <Note tone="warning">{tokens(total.unpriced)} tokens had no price, so the true cost is higher.</Note> : null}
        <T dim>input {tokens(total.input)} · output {tokens(total.output)} · to cache {tokens(total.cacheWrite)} · cached {tokens(total.cached)}</T>
        <T dim>{mix(total)}</T>
        {(['step', 'agent', 'model', 'host'] as const).filter(grouping => grouping !== own).map(grouping => {
          const found = groups(mine, grouping);
          if (found.length < 2 && grouping !== 'step') return null;
          return <View key={grouping}>
            <SectionHeader title={`by ${grouping}`} count={found.length} />
            {found.slice(0, 8).map(group => <ListRow key={group.id} title={own === 'mission' && grouping === 'step' ? group.id.split('/').pop() : label(group.id, names, rows)}
              right={<T color={theme.accent}>{cost(group.total)}  <T dim>{tokens(group.total.tokens)} · {tokenShare(group.total.cached, group.total.tokens)}</T></T>}
              onPress={() => navigation.push('UsageDetail', { id: group.id, hours })} />)}
            {found.length > 8 ? <Note>and {found.length - 8} more</Note> : null}
          </View>;
        })}
      </>}
      {limits.map(limit => {
        const line = limitLine(limit, now);
        return <View key={limit.account}>
          <SectionHeader title="limits" />
          <T>weekly <T bold color={tone[line.weekly.tone]}>{line.weekly.text}</T>{line.resets ? ` ${line.resets}` : ''} · 5-hour <T bold color={tone[line.fiveHour.tone]}>{line.fiveHour.text}</T></T>
          <T dim>{line.measured} by {label(limit.measured_by, names, rows)} on {limit.host} · {limit.seats.length} seat{limit.seats.length === 1 ? '' : 's'}{limit.plan ? ` · plan ${limit.plan}` : ''}</T>
        </View>;
      })}
    </ScrollView>
  </Screen>;
}

/** Fleet's usage card: each account's spend today and its weekly share; it opens Usage. */
export function UsageCard() {
  const navigation = useNavigation<NativeStackNavigationProp<StackParams>>();
  const { period, issue } = useUsage(PERIODS[0]);
  const names = useNames();
  const now = Date.now();
  const rows = period?.rows ?? [];
  const found = accounts(rows, period?.limits ?? []);
  return <View>
    <SectionHeader title="usage · today" />
    {issue ? <Note tone="warning">{issue}</Note> : null}
    {!period && !issue ? <Note>Loading usage…</Note> : null}
    {found.map(({ id, total: spent, limit }) => {
      const weekly = limit ? limitLine(limit, now).weekly : null;
      return <ListRow key={id} title={label(id, names, rows)} onPress={() => navigation.navigate('Usage')}
        right={<T color={theme.accent}>{cost(spent)}</T>}
        second={weekly ? <T dim>weekly <T bold color={tone[weekly.tone]}>{weekly.text}</T>{spent.tokens ? ` · ${tokenShare(spent.cached, spent.tokens)} cached` : ''}</T> : `${tokens(spent.tokens)} tokens · ${tokenShare(spent.cached, spent.tokens)} cached`} />;
    })}
    {period ? <ListRow title={<T color={theme.accent}>All usage ›</T>} onPress={() => navigation.navigate('Usage')} /> : null}
  </View>;
}

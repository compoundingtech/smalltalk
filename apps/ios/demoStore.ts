// Demo mode: the invented fleet from fixtures/clients/demo-world.json, the same one `stui demo`
// shows. Every button works on this device and nothing is sent anywhere.

import { useCallback, useMemo, useRef, useState } from 'react';
import demoJson from '../../fixtures/clients/demo-world.json';
import type { Attention, Entry, Mission, World } from './clientView';
import { decodeWorld, items } from './clientView.ts';
import { clock } from './harnessConversation.ts';
import { aboutTitle, chatTarget } from './screenModel.ts';
import type { TerminalLine, TerminalRun } from '../../clients/typescript/st3-client/Models.generated';
import type { CardAction, MissionAction, NewMission, Store, TerminalHandlers, TerminalView } from './store';
import { colors } from './theme';

export const demoWorld = (): World => decodeWorld(demoJson);

/** The invented terminal the demo opens for any agent, the one `stui demo` shows. */
export function demoTerminal(name: string): TerminalView {
  const dim = colors.overlay0, accent = colors.peach;
  const line = (row: number, ...runs: TerminalRun[]): TerminalLine => ({ row, runs, text: runs.map(run => run.text).join(''), redacted: false, truncated: false });
  const rows: TerminalRun[][] = [
    [{ text: `╭─ ${name} ─────────────────────────────────────────╮`, fg: dim }],
    [{ text: '│ ', fg: dim }, { text: '✻ Working on the cut-over review', fg: accent, bold: true }],
    [{ text: '│', fg: dim }],
    [{ text: '│ ⏺ Read(docs/compare-report.md)' }],
    [{ text: '│   ⎿ 42 lines', fg: dim }],
    [{ text: '│ ⏺ Waiting for a person to approve the cut-over.' }],
    [{ text: '╰────────────────────────────────────────────────────╯', fg: dim }],
    [],
    [{ text: '> ', fg: accent, bold: true }, { text: '█' }],
    [],
    [{ text: '  demo: this screen is invented and keys are not sent', fg: dim }],
  ];
  return { lines: rows.map((runs, row) => line(row, ...runs)), columns: 56, rows: rows.length };
}

const RESOLVED: Partial<Record<CardAction, string>> = {
  approve: 'Approved', cancel: 'Launch cancelled', reject: 'Revision rejected', resolve: 'Marked resolved', read: 'Marked read',
};

function append(world: World, agent: string, entries: Entry[]): World {
  const conversation = world.conversations[agent];
  if (conversation?.state !== 'ready') return world;
  return { ...world, conversations: { ...world.conversations, [agent]: { state: 'ready', value: [...conversation.value, ...entries] } } };
}

/** Take an answered item off Home and let its mission move on. */
function resolve(world: World, id: string): World {
  if (world.attention.state !== 'ready' || world.missions.state !== 'ready') return world;
  return {
    ...world,
    attention: { state: 'ready', value: world.attention.value.filter(item => item.id !== id) },
    missions: {
      state: 'ready',
      value: world.missions.value.map(mission => mission.decision !== id ? mission : {
        ...mission,
        decision: null,
        word: 'working',
        steps: mission.steps.map(step => step.state === 'needs_you' ? { ...step, state: 'working', note: 'you answered; the agent is on it' } : step),
      }),
    },
  };
}

export function useDemoStore(leave: () => Promise<void>): Store {
  const [world, setWorld] = useState(demoWorld);
  const [snoozed, setSnoozed] = useState<ReadonlySet<string>>(new Set());
  const [notice, setNotice] = useState<string | null>(null);
  const counter = useRef(0);
  const nextId = (prefix: string) => `${prefix}-${++counter.current}`;
  const now = () => clock(new Date().toISOString());

  const act = useCallback(async (item: Attention, action: CardAction) => {
    setWorld(current => resolve(current, item.id));
    setNotice(`${RESOLVED[action] ?? 'Sent to the agent'} · demo: nothing was sent`);
    return true;
  }, []);

  const discuss = useCallback(async (item: Attention, to: string, text: string) => {
    setWorld(current => {
      const name = items(current.agents).find(agent => agent.id === to)?.name ?? chatTarget(current, item)?.name ?? to;
      const subject = aboutTitle(item);
      return append(current, to, [
        { id: nextId('about'), at: now(), body: { kind: 'mail', value: { from: 'you', to: name, subject, body: text } } },
        { id: nextId('about'), at: now(), body: { kind: 'mail', value: { from: name, to: 'you', subject, body: 'Good question. Here is what I know, and what I would need from you to go on. (demo reply)' } } },
      ]);
    });
    setNotice('Sent · demo: nothing left this device');
    return true;
  }, []);

  const send = useCallback(async (agent: string, text: string) => {
    setWorld(current => {
      const name = items(current.agents).find(candidate => candidate.id === agent)?.name ?? agent;
      return append(current, agent, [{ id: nextId('you'), at: now(), body: { kind: 'mail', value: { from: 'you', to: name, subject: '', body: text } } }]);
    });
    setNotice('Sent · demo: nothing left this device');
    return true;
  }, []);

  const missionAction = useCallback(async (_mission: Mission, action: MissionAction) => {
    setNotice(action === 'cancel' ? 'Run cancelled · demo: nothing was sent'
      : `${action === 'retry' ? 'Retry the step' : 'Restart the agent'} · demo: nothing was sent`);
  }, []);

  const openTerminal = useCallback((agent: { name: string }, handlers: TerminalHandlers) => {
    handlers.onScreen(demoTerminal(agent.name));
    handlers.onIssue('');
    return {
      close: () => {},
      send: async () => { setNotice('demo: keys are not sent'); return false; },
    };
  }, []);

  const createLaunch = useCallback(async (_form: NewMission) => {
    setNotice('Launch created · demo: nothing was sent');
    return true;
  }, []);

  const revokeDevice = useCallback(async (id: string) => {
    setWorld(current => current.devices.state !== 'ready' ? current : { ...current, devices: { state: 'ready', value: current.devices.value.filter(device => device.id !== id) } });
    setNotice('Device revoked · demo: nothing was sent');
    return true;
  }, []);

  return useMemo<Store>(() => ({
    mode: 'demo',
    world,
    snoozed,
    notice,
    clearNotice: () => setNotice(null),
    refresh: async () => {},
    watchConversation: () => () => {},
    watchAttention: () => () => {},
    act,
    discuss,
    send,
    snooze: id => { setSnoozed(current => new Set([...current, id])); setNotice('Put off until later · demo, this device only'); },
    missionAction,
    openTerminal,
    canTypeInTerminals: true,
    createLaunch,
    revokeDevice,
    leave,
    connection: { gateway: null, person: world.person, status: 'Demo · invented data, nothing is sent' },
  }), [world, snoozed, notice, act, discuss, send, missionAction, openTerminal, createLaunch, revokeDevice, leave]);
}

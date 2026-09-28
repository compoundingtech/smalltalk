// What the screens can read and do. The demo store and the live store both implement it, so
// every screen works the same on invented data and on the graph.

import { createContext, useContext } from 'react';
import type { TerminalLine } from '../../clients/typescript/st3-client/Models.generated';
import type { Agent, Attention, Mission, World } from './clientView';

/** A card's buttons, named by what the person means rather than by st action. */
export type CardAction =
  | 'approve' // review, feedback ("looks good"), launch, revision
  | 'send-back' // review: request changes with the text box
  | 'feedback' // feedback: send the text box
  | 'ask-changes' // launch, revision: ask for changes with the text box
  | 'cancel' // launch
  | 'reject' // revision
  | 'resolve' // fault
  | 'reply' // message: send the text box
  | 'read'; // message

export type MissionAction = 'retry' | 'restart' | 'cancel';

/** One terminal screen: its lines with colours and styles, and its size. */
export type TerminalView = { lines: TerminalLine[]; columns: number; rows: number; cursor?: { row: number; column: number; visible: boolean } };

export type TerminalHandlers = {
  /** Each screen replaces the one before it. */
  onScreen: (screen: TerminalView) => void;
  /** A problem to show; an empty string clears it. */
  onIssue: (issue: string) => void;
};

/** An open terminal. Closing it detaches. */
export type TerminalSession = { close(): void; send(mode: 'line' | 'key', value: string): Promise<boolean> };

/** The New mission form: it creates a launch, which a planner turns into a proposal on Home. */
export type NewMission = { title: string; request: string; mission: string; workspace: string };

export type Store = {
  mode: 'demo' | 'live';
  world: World;
  /** Items put off until later on this device. */
  snoozed: ReadonlySet<string>;
  /** A short line about the last thing that happened, cleared by the screen after a while. */
  notice: string | null;
  clearNotice(): void;
  refresh(): Promise<void>;
  /** Keep this agent's conversation fresh while a screen shows it. Returns the unsubscribe. */
  watchConversation(agent: string): () => void;
  /** Load what a card needs (a launch preview, the message behind an item) while it shows. */
  watchAttention(item: string): () => void;
  /** Resolves true when st accepted it (or the demo pretended to). */
  act(item: Attention, action: CardAction, text?: string): Promise<boolean>;
  /** "Chat about this": a new message to `to` titled after the item, with the item as context. */
  discuss(item: Attention, to: string, text: string): Promise<boolean>;
  send(agent: string, text: string): Promise<boolean>;
  snooze(item: string): void;
  missionAction(mission: Mission, action: MissionAction): Promise<void>;
  /** Attach to an agent's terminal and follow its screen until closed. */
  openTerminal(agent: Agent, handlers: TerminalHandlers): TerminalSession;
  /** Whether this device may type into terminals (`terminal.input`). */
  canTypeInTerminals: boolean;
  createLaunch(form: NewMission): Promise<boolean>;
  revokeDevice(id: string): Promise<boolean>;
  /** Leave demo mode, or forget the paired device. */
  leave(): Promise<void>;
  /** For the settings screen: where this app reads from. */
  connection: { gateway: string | null; person: string; status: string };
};

export const StoreContext = createContext<Store | null>(null);

export function useStore(): Store {
  const store = useContext(StoreContext);
  if (!store) throw new Error('useStore outside StoreContext');
  return store;
}

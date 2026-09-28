// What the screens can read and do. The demo store and the live store both implement it, so
// every screen works the same on invented data and on the graph.

import { createContext, useContext } from 'react';
import type { Attention, Mission, World } from './clientView';

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

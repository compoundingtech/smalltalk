import type { RootScreen } from './navigation';

/** Push even when the parent is already a Conversation: Back must restore the parent. */
export const openSubagentConversation = (navigation: Pick<RootScreen<'Conversation'>['navigation'], 'push'>, sessionId: string, title: string): void => {
  navigation.push('Conversation', { target: sessionId, sessionId, title });
};

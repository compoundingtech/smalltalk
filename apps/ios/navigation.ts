import { createNavigationContainerRef, type NavigatorScreenParams } from '@react-navigation/native';
import type { NativeStackScreenProps } from '@react-navigation/native-stack';
import type { Tab } from './tabs';

// Each tab is a native UINavigationController (react-native-screens) inside one native
// UITabBarController. Every tab's stack can push every detail, so a conversation opened from
// Home stays in Home's stack and the back button returns there.
export type StackParams = {
  HomeRoot: undefined;
  AgentsRoot: undefined;
  MissionsRoot: undefined;
  FleetRoot: undefined;
  Conversation: { target: string; sessionId?: string; title?: string };
  Terminal: { terminalId: string; title?: string };
  Mission: { id: string; title?: string };
  Attention: { id: string };
  Launch: { id: string };
  NewMission: undefined;
  History: undefined;
};
export type TabParams = { [K in Tab]: NavigatorScreenParams<StackParams> | undefined };

export const ROOTS: Record<Tab, keyof StackParams> = { Home: 'HomeRoot', Agents: 'AgentsRoot', Missions: 'MissionsRoot', Fleet: 'FleetRoot' };
/** Details that take the whole screen: the tab bar hides while they are on top, as in Messages. */
export const FULL_SCREEN: ReadonlyArray<keyof StackParams> = ['Conversation', 'Terminal'];

export type RootParams = StackParams;
export type RootScreen<K extends keyof StackParams> = NativeStackScreenProps<StackParams, K>;
export const navigationRef = createNavigationContainerRef<TabParams>();

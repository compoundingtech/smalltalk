// Where a reference goes. Every name on screen is a link: a tap opens a sheet with a summary,
// Go to and Message; Go to switches to the subject's tab and pushes its screen.

import { useNavigation, type NavigationProp, type ParamListBase } from '@react-navigation/native';
import { useCallback } from 'react';

export type Nav = NavigationProp<ParamListBase>;

export function routeFor(subject: string): { tab: string; screen: string; params: Record<string, string> } | null {
  if (subject.startsWith('agent/') || subject.startsWith('session/')) return { tab: 'AgentsTab', screen: 'Agent', params: { id: subject } };
  if (subject.startsWith('mission/')) return { tab: 'MissionsTab', screen: 'Mission', params: { id: subject } };
  if (subject.startsWith('attention/')) return { tab: 'HomeTab', screen: 'Attention', params: { id: subject } };
  return null;
}

export function goTo(navigation: Nav, subject: string) {
  const route = routeFor(subject);
  if (!route) return;
  navigation.navigate('Tabs', { screen: route.tab, params: { screen: route.screen, params: route.params, initial: false } });
}

export function useLinks() {
  const navigation = useNavigation<Nav>();
  return {
    navigation,
    peek: useCallback((subject: string) => navigation.navigate('Peek', { subject }), [navigation]),
    goTo: useCallback((subject: string) => goTo(navigation, subject), [navigation]),
  };
}

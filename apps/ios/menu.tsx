import type { ReactNode } from 'react';
import { MenuView } from '@react-native-menu/menu';

// A native context menu (UIMenu) on long press, from @react-native-menu/menu. The row's own tap
// still opens it; the menu offers the other things a person can do with it.
export type MenuAction = { id: string; title: string; symbol?: string; destructive?: boolean; run: () => void };

export function ContextMenu({ actions, title, children }: { actions: MenuAction[]; title?: string; children: ReactNode }) {
  if (!actions.length) return <>{children}</>;
  return <MenuView
    title={title}
    shouldOpenOnLongPress
    actions={actions.map(action => ({ id: action.id, title: action.title, image: action.symbol, attributes: action.destructive ? { destructive: true } : undefined }))}
    onPressAction={({ nativeEvent }) => actions.find(action => action.id === nativeEvent.event)?.run()}
  >
    {children}
  </MenuView>;
}

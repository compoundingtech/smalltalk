import { cloneElement, isValidElement, type ReactElement } from 'react';
import { ActionSheetIOS } from 'react-native';

// What else a person can do with a row, on long press, as an action sheet. The row keeps its
// own tap. A native UIMenu wrapped around the row swallowed that tap on iOS 27, so a list of
// agents could not be opened (Nathan, 2026-10-01).
export type MenuAction = { id: string; title: string; symbol?: string; destructive?: boolean; run: () => void };

export function ContextMenu({ actions, title, children }: { actions: MenuAction[]; title?: string; children: ReactElement<{ onLongPress?: () => void }> }) {
  if (!actions.length || !isValidElement(children)) return children;
  const show = () => ActionSheetIOS.showActionSheetWithOptions(
    {
      title,
      options: [...actions.map(action => action.title), 'Cancel'],
      cancelButtonIndex: actions.length,
      destructiveButtonIndex: actions.flatMap((action, index) => action.destructive ? [index] : []),
    },
    index => actions[index]?.run(),
  );
  return cloneElement(children, { onLongPress: show });
}

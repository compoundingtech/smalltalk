import { useLayoutEffect } from 'react';
import { StyleSheet, TextInput } from 'react-native';
import type { RootScreen } from '../navigation';
import { fonts, theme } from '../theme';
import { LINE } from '../ui';

// One conversation entry's text in a native text view, where any part can be selected and
// copied. The keyboard never comes up and the text never changes.
export function SelectTextScreen({ route, navigation }: RootScreen<'SelectText'>) {
  const { text, title } = route.params;
  useLayoutEffect(() => { if (title) navigation.setOptions({ title }); }, [navigation, title]);
  return <TextInput
    style={styles.text}
    value={text}
    multiline
    autoFocus={false}
    showSoftInputOnFocus={false}
    scrollEnabled
    contextMenuHidden={false}
    autoCorrect={false}
    spellCheck={false}
    onChangeText={() => undefined}
    textAlignVertical="top"
  />;
}

const styles = StyleSheet.create({
  text: { flex: 1, backgroundColor: theme.base, color: theme.text, fontFamily: fonts.regular, fontSize: 14, lineHeight: LINE, padding: 16 },
});

import * as React from 'react';
import { Text, View } from 'react-native';
import { captureJsError, captureRootMounted, handleInitialDiagnosticLink, subscribeDiagnosticBoundaryInjection } from './diagnostics';
import { theme } from './theme';

/** Debug-only child deliberately throws during React render, exercising the real boundary. */
class DiagnosticInjection extends React.Component {
  state = { fail: false };
  private unsubscribe: (() => void) | undefined;
  componentDidMount(): void {
    this.unsubscribe = subscribeDiagnosticBoundaryInjection(() => this.setState({ fail: true }));
    handleInitialDiagnosticLink();
  }
  componentWillUnmount(): void { this.unsubscribe?.(); }
  render(): React.ReactNode {
    if (this.state.fail) throw new TypeError('Diagnostic injection: private render message');
    return null;
  }
}

export class DiagnosticsBoundary extends React.Component<React.PropsWithChildren, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError(): { failed: boolean } { return { failed: true }; }
  componentDidMount(): void { captureRootMounted(); }
  componentDidCatch(error: unknown): void { captureJsError(error, true); }
  render(): React.ReactNode {
    if (this.state.failed) return <View style={{ flex: 1, justifyContent: 'center', padding: 24, backgroundColor: theme.base }}>
      <Text accessibilityRole="header" style={{ color: theme.text, fontSize: 20 }}>The app could not continue</Text>
      <Text style={{ color: theme.text, marginTop: 12 }}>Close and reopen Smalltalk. If device storage is available, a redacted diagnostic will be retained for the next paired connection.</Text>
    </View>;
    return <>{__DEV__ ? <DiagnosticInjection /> : null}{this.props.children}</>;
  }
}

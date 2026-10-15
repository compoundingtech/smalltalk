import * as React from 'react';
import type { St3Client } from '../../clients/typescript/st3-client';
import { connectDiagnostics, disconnectDiagnostics } from './diagnostics';

type Props = { client: St3Client | null; url: string; credential: string | null; enabled: boolean };
/** A lifecycle adapter, not derived React state. It only provides existing paired
 * context; diagnostics owns persistence, scheduling, and native report transfer. */
export class DiagnosticsConnection extends React.Component<Props> {
  componentDidMount(): void { this.connect(); }
  componentDidUpdate(previous: Props): void {
    if (previous.client !== this.props.client || previous.url !== this.props.url || previous.credential !== this.props.credential || previous.enabled !== this.props.enabled) this.connect();
  }
  componentWillUnmount(): void { disconnectDiagnostics(this); }
  private connect(): void {
    connectDiagnostics(this, this.props.client, this.props.url, this.props.credential, this.props.enabled);
  }
  render(): React.ReactNode { return null; }
}

import { requireOptionalNativeModule } from 'expo';
import type { ClientDiagnosticEvent } from '../../../../clients/typescript/st3-client';

export type NativeReport = ClientDiagnosticEvent;

export type LaunchContext = {
  launch_id: string;
  started_at_unix_ms: number;
  app_version: string;
  native_build: string;
  os_version: string;
};

export type StDiagnosticsModule = {
  /** A non-destructive snapshot; reports expire after seven days and oldest reports may be evicted. */
  getPendingReports(): Promise<NativeReport[]>;
  /** Call only after these IDs are durably retained in the JavaScript queue. */
  acknowledge(ids: string[]): Promise<void>;
  /** The same process launch UUID used by the pre-JavaScript native marker. */
  launchContext(): LaunchContext;
};

const StDiagnostics = requireOptionalNativeModule<StDiagnosticsModule>('StDiagnostics');
export default StDiagnostics;

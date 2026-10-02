// On-device dictation (modules/st-dictation/ios): the words so far and the microphone's level.
// The native module is absent in a build without it (or before iOS 26); then nothing is offered.
import { requireOptionalNativeModule } from 'expo';

type Subscription = { remove(): void };
type Native = {
  isAvailable(): boolean;
  start(locale?: string): Promise<void>;
  stop(): Promise<string>;
  addListener(event: 'onText', listener: (event: { text: string; final: boolean }) => void): Subscription;
  addListener(event: 'onLevel', listener: (event: { level: number }) => void): Subscription;
  addListener(event: 'onError', listener: (event: { message: string }) => void): Subscription;
};

const native = requireOptionalNativeModule<Native>('StDictation');

export const dictationAvailable = (): boolean => !!native?.isAvailable();

export type DictationHandlers = { onText: (text: string) => void; onLevel: (level: number) => void; onError: (message: string) => void };

/** Start listening; the returned stop gives everything heard. */
export async function startDictation(handlers: DictationHandlers): Promise<() => Promise<string>> {
  if (!native) throw new Error('Dictation is not in this build');
  const subscriptions = [
    native.addListener('onText', event => handlers.onText(event.text)),
    native.addListener('onLevel', event => handlers.onLevel(event.level)),
    native.addListener('onError', event => handlers.onError(event.message)),
  ];
  try {
    await native.start();
  } catch (error) {
    subscriptions.forEach(subscription => subscription.remove());
    throw error;
  }
  return async () => {
    try { return await native.stop(); } finally { subscriptions.forEach(subscription => subscription.remove()); }
  };
}

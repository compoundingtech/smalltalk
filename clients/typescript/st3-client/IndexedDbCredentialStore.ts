/** Native paired-session bearers, scoped to a gateway base URL within this browser origin.
 * IndexedDB is persistence, not an XSS boundary: same-origin JavaScript can read these bearers. */
export class IndexedDbCredentialStore {
    constructor(private readonly databaseName = 'st3-client-credentials') {}

    /** Suitable for `St3Client`'s asynchronous `credential` callback. */
    async get(gatewayUrl: string): Promise<string | undefined> {
        return this.transact(gatewayUrl, 'readonly', (store, key) => store.get(key));
    }

    /** Persist `completePairing(...).value.credential` after successful native pairing. */
    async set(gatewayUrl: string, credential: string): Promise<void> {
        await this.transact(gatewayUrl, 'readwrite', (store, key) => store.put(credential, key));
    }

    /** Remove the local bearer on disconnect or after session revocation. */
    async delete(gatewayUrl: string): Promise<void> {
        await this.transact(gatewayUrl, 'readwrite', (store, key) => store.delete(key));
    }

    private async transact<T>(gatewayUrl: string, mode: IDBTransactionMode, operation: (store: IDBObjectStore, key: string) => IDBRequest<T>): Promise<T> {
        const gateway = new URL(gatewayUrl);
        const key = gateway.origin + gateway.pathname.replace(/\/+$/, '');
        const database = await new Promise<IDBDatabase>((resolve, reject) => {
            const request = indexedDB.open(this.databaseName, 1);
            request.onupgradeneeded = () => { request.result.createObjectStore('credentials'); };
            request.onsuccess = () => resolve(request.result);
            request.onerror = () => reject(request.error);
        });
        try {
            return await new Promise<T>((resolve, reject) => {
                const transaction = database.transaction('credentials', mode);
                const request = operation(transaction.objectStore('credentials'), key);
                transaction.oncomplete = () => resolve(request.result);
                transaction.onabort = () => reject(transaction.error ?? request.error);
                transaction.onerror = () => reject(transaction.error ?? request.error);
            });
        } finally {
            database.close();
        }
    }
}

/**
 * In-memory `DurableObjectState`-like fake for unit tests.
 *
 * The real Workers runtime exposes far more (transactional storage,
 * hibernation hooks, alarms with retry, blockConcurrencyWhile, etc.). We
 * only fake the surface area the PDX-20 DOs depend on. The fake is
 * deliberately synchronous-flavoured (Promises that resolve in microtasks)
 * so concurrent-monotonic tests can exercise the in-memory ordering
 * primitives the production code relies on.
 */
export class InMemoryStorage {
  private readonly map = new Map<string, unknown>();
  private alarmAt: number | null = null;

  async get<T>(key: string): Promise<T | undefined> {
    return this.map.get(key) as T | undefined;
  }

  async put<T>(key: string, value: T): Promise<void> {
    // Deep-clone via JSON so callers can't mutate the stored object after
    // put() — matches the real runtime's "deep-copy on persist" semantics.
    this.map.set(key, JSON.parse(JSON.stringify(value)));
  }

  async delete(key: string): Promise<boolean> {
    return this.map.delete(key);
  }

  async setAlarm(scheduledTime: number | Date): Promise<void> {
    this.alarmAt =
      scheduledTime instanceof Date ? scheduledTime.getTime() : scheduledTime;
  }

  async getAlarm(): Promise<number | null> {
    return this.alarmAt;
  }

  /** Pass-through transaction — sufficient because put/get already serialize. */
  async transaction<T>(fn: (txn: InMemoryStorage) => Promise<T>): Promise<T> {
    return fn(this);
  }

  /** Test helper. */
  raw(): Map<string, unknown> {
    return this.map;
  }
}

export interface InMemoryDOState {
  storage: InMemoryStorage;
  acceptWebSocket?: (ws: WebSocket, tags?: string[]) => void;
  getWebSockets?: (tag?: string) => WebSocket[];
}

export function makeInMemoryState(): InMemoryDOState {
  return { storage: new InMemoryStorage() };
}

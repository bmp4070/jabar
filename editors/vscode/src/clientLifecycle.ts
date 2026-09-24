export interface ManagedClient {
  stop(): Promise<void>;
}

/** Serializes startup, configuration restarts, and extension shutdown. */
export class ClientLifecycle<T extends ManagedClient> {
  private current: T | undefined;
  private queue: Promise<void> = Promise.resolve();
  private shuttingDown = false;

  constructor(private readonly startClient: () => Promise<T | undefined>) {}

  get client(): T | undefined {
    return this.current;
  }

  start(): Promise<void> {
    return this.enqueue(async () => {
      if (!this.shuttingDown) {
        this.current = await this.startClient();
      }
    });
  }

  restart(): Promise<boolean> {
    return this.enqueue(async () => {
      if (this.shuttingDown) {
        return false;
      }

      const previous = this.current;
      if (previous) {
        // Keep the client reachable if stop fails. A later settings change can
        // retry instead of starting a second server beside it.
        await previous.stop();
        if (this.current === previous) {
          this.current = undefined;
        }
      }

      // shutdown() sets this before joining the queue, including while the
      // stop above is pending.
      if (this.shuttingDown) {
        return false;
      }
      const replacement = await this.startClient();
      if (!replacement) {
        return false;
      }
      this.current = replacement;
      return true;
    });
  }

  shutdown(): Promise<void> {
    this.shuttingDown = true;
    return this.enqueue(async () => {
      const previous = this.current;
      if (previous) {
        await previous.stop();
        if (this.current === previous) {
          this.current = undefined;
        }
      }
    });
  }

  private enqueue<R>(operation: () => Promise<R>): Promise<R> {
    const result = this.queue.then(operation, operation);
    // Keep the internal tail usable after an operation fails. The returned
    // promise still rejects so the extension can log the individual failure.
    this.queue = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }
}

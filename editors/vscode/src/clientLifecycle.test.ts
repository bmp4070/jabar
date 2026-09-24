import { strict as assert } from "node:assert";
import { test } from "node:test";

import { ClientLifecycle } from "./clientLifecycle";

function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

test("a restart waits for initial startup before stopping the client", async () => {
  const startup = deferred();
  const clients: Array<{ stopCalls: number; stop(): Promise<void> }> = [];
  const lifecycle = new ClientLifecycle(async () => {
    const client = {
      stopCalls: 0,
      async stop() {
        this.stopCalls += 1;
      },
    };
    clients.push(client);
    if (clients.length === 1) {
      await startup.promise;
    }
    return client;
  });

  const initial = lifecycle.start();
  const restart = lifecycle.restart();
  await Promise.resolve();
  assert.equal(clients.length, 1);
  assert.equal(clients[0].stopCalls, 0);

  startup.resolve();
  await initial;
  assert.equal(await restart, true);
  assert.equal(clients.length, 2);
  assert.equal(clients[0].stopCalls, 1);
  assert.equal(lifecycle.client, clients[1]);
});

test("shutdown during a restart does not launch a replacement", async () => {
  const stopping = deferred();
  let starts = 0;
  let stopStarted!: () => void;
  const stopWasStarted = new Promise<void>((resolve) => {
    stopStarted = resolve;
  });
  const lifecycle = new ClientLifecycle(async () => {
    starts += 1;
    return {
      async stop() {
        stopStarted();
        await stopping.promise;
      },
    };
  });

  await lifecycle.start();
  const restart = lifecycle.restart();
  await stopWasStarted;
  const shutdown = lifecycle.shutdown();
  stopping.resolve();

  assert.equal(await restart, false);
  await shutdown;
  assert.equal(starts, 1);
  assert.equal(lifecycle.client, undefined);
});

test("a failed stop blocks replacement but the queue can retry", async () => {
  let starts = 0;
  let stopAttempts = 0;
  const lifecycle = new ClientLifecycle(async () => {
    starts += 1;
    return {
      async stop() {
        stopAttempts += 1;
        if (stopAttempts === 1) {
          throw new Error("still starting");
        }
      },
    };
  });

  await lifecycle.start();
  await assert.rejects(lifecycle.restart(), /still starting/);
  assert.equal(starts, 1);
  assert.equal(await lifecycle.restart(), true);
  assert.equal(starts, 2);
});

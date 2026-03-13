import type { Env } from "../../src/types";

type KvValue = {
  value: string;
  expiration?: number;
};

class MockKV {
  private readonly store = new Map<string, KvValue>();

  async get<T = string>(key: string, type?: "text" | "json" | "arrayBuffer" | "stream"): Promise<T | null> {
    const entry = this.store.get(key);
    if (!entry) {
      return null;
    }

    if (entry.expiration && entry.expiration < Math.floor(Date.now() / 1000)) {
      this.store.delete(key);
      return null;
    }

    if (type === "json") {
      return JSON.parse(entry.value) as T;
    }
    if (type === "arrayBuffer") {
      return new TextEncoder().encode(entry.value).buffer as T;
    }
    if (type === "stream") {
      return new Response(entry.value).body as T;
    }
    return entry.value as T;
  }

  async put(key: string, value: string, options?: { expirationTtl?: number }): Promise<void> {
    const expiration = options?.expirationTtl
      ? Math.floor(Date.now() / 1000) + options.expirationTtl
      : undefined;
    this.store.set(key, {
      value,
      expiration
    });
  }
}

export function createEnv(): Env {
  return {
    UNFURL_CACHE: new MockKV() as unknown as KVNamespace
  };
}

export function createExecutionContext(): ExecutionContext & { drain: () => Promise<void> } {
  const waits: Promise<unknown>[] = [];

  return {
    waitUntil(promise: Promise<unknown>): void {
      waits.push(promise);
    },
    passThroughOnException(): void {
      return;
    },
    async drain(): Promise<void> {
      await Promise.all(waits);
    }
  } as ExecutionContext & { drain: () => Promise<void> };
}
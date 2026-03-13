import type { FetchPageOptions } from "../types";
import { USER_AGENTS } from "./user-agents";
import { AppError, DEFAULT_FETCH_TIMEOUT_MS } from "../utils";

export async function fetchPage(url: string, options: FetchPageOptions = {}): Promise<Response> {
  const fetchImpl = options.fetchImpl ?? fetch;
  const timeoutMs = options.timeoutMs ?? DEFAULT_FETCH_TIMEOUT_MS;
  const maxAttempts = Math.max(1, Math.min(options.maxAttempts ?? 3, USER_AGENTS.length));
  const agents = USER_AGENTS.slice(0, maxAttempts);
  let lastResponse: Response | undefined;
  let lastError: unknown;

  for (const [index, agent] of agents.entries()) {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), timeoutMs);

    try {
      const response = await fetchImpl(url, {
        redirect: "follow",
        signal: controller.signal,
        headers: {
          "user-agent": agent.value,
          accept: "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8"
        }
      });

      lastResponse = response;
      if ((response.status === 403 || response.status === 429) && index < agents.length - 1) {
        continue;
      }

      return response;
    } catch (error) {
      lastError = error;
      if (index === agents.length - 1) {
        break;
      }
    } finally {
      clearTimeout(timeoutId);
    }
  }

  if (lastResponse) {
    return lastResponse;
  }

  if (lastError instanceof Error && lastError.name === "AbortError") {
    throw new AppError(504, "FETCH_TIMEOUT", `Fetching ${url} timed out after ${timeoutMs}ms`);
  }

  throw new AppError(502, "FETCH_FAILED", `Unable to fetch ${url}`);
}
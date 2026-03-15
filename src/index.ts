import { handleImageProxy } from "./handlers/image-proxy";
import { handleUnfurl } from "./handlers/unfurl";
import type { Env } from "./types";
import { AppError, createCorsHeaders, createErrorResponse, jsonResponse, withCors } from "./utils";

function withoutBody(response: Response): Response {
  return new Response(null, {
    status: response.status,
    statusText: response.statusText,
    headers: response.headers
  });
}

async function routeRequest(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
  const url = new URL(request.url);

  if (request.method === "OPTIONS") {
    return new Response(null, {
      status: 204,
      headers: createCorsHeaders()
    });
  }

  if (request.method !== "GET" && request.method !== "HEAD") {
    throw new AppError(405, "METHOD_NOT_ALLOWED", "Only GET and HEAD are supported");
  }

  if (url.pathname === "/health") {
    return jsonResponse(
      {
        status: "success",
        data: {
          title: null,
          description: null,
          image: null,
          url: "health://ok",
          author: null,
          publisher: null,
          date: null,
          lang: null,
          logo: null,
          video: null,
          audio: null
        },
        headers: {
          "x-cache-status": "MISS",
          "x-response-time": "0ms"
        }
      },
      200,
      {
        "cache-control": "no-store"
      }
    );
  }

  if (url.pathname === "/api") {
    return handleUnfurl(request, env, ctx);
  }

  if (url.pathname === "/proxy/image") {
    return handleImageProxy(request, env, ctx);
  }

  throw new AppError(404, "NOT_FOUND", "Route not found");
}

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const startedAt = Date.now();

    try {
      const response = await routeRequest(request, env, ctx);
      return withCors(request.method === "HEAD" ? withoutBody(response) : response);
    } catch (error) {
      return withCors(createErrorResponse(error, startedAt));
    }
  }
};

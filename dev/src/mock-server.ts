import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { randomUUID } from "node:crypto";
import { createOpenAICompatible } from "@ai-sdk/openai-compatible";

type JsonObject = Record<string, unknown>;
type EndpointType = "chat.completions" | "responses";

type RequestContext = {
  provider: string;
  endpoint: EndpointType;
  payload: JsonObject;
};

type ResponseContext = RequestContext & {
  prompt: string;
  model: string;
  answer: string;
};

type ProviderHook = {
  beforeRequest?: (ctx: RequestContext) => JsonObject;
  afterResponse?: (ctx: ResponseContext) => string;
};

const PORT = Number(process.env.MOCK_PORT ?? 4010);
const HOST = process.env.MOCK_HOST ?? "127.0.0.1";
const MODEL_FALLBACK = process.env.MOCK_MODEL ?? "gpt-mock-1";
const DEFAULT_PROVIDER = process.env.MOCK_DEFAULT_PROVIDER ?? "openai";
const STREAM_DELAY_MS = Number(process.env.MOCK_STREAM_DELAY_MS ?? 35);

const providerHooks: Record<string, ProviderHook> = {
  openai: {
    beforeRequest(ctx) {
      return {
        ...ctx.payload,
        model: modelFrom(ctx.payload),
      };
    },
    afterResponse(ctx) {
      return `[openai] ${ctx.answer}`;
    },
  },
  anthropic: {
    beforeRequest(ctx) {
      return {
        ...ctx.payload,
        metadata: {
          ...(typeof ctx.payload.metadata === "object" && ctx.payload.metadata !== null
            ? (ctx.payload.metadata as JsonObject)
            : {}),
          provider_hint: "anthropic-mock-hook",
        },
      };
    },
    afterResponse(ctx) {
      return `[anthropic] ${ctx.answer}`;
    },
  },
  deepseek: {
    beforeRequest(ctx) {
      return {
        ...ctx.payload,
        temperature: typeof ctx.payload.temperature === "number" ? ctx.payload.temperature : 0.2,
      };
    },
    afterResponse(ctx) {
      return `[deepseek] ${ctx.answer}`;
    },
  },
};

const sdkProviderCache = new Map<string, ReturnType<typeof createOpenAICompatible>>();

function getSdkProvider(provider: string): ReturnType<typeof createOpenAICompatible> {
  const cached = sdkProviderCache.get(provider);
  if (cached) {
    return cached;
  }
  const sdkProvider = createOpenAICompatible({
    name: provider,
    apiKey: `mock-${provider}-key`,
    baseURL: `http://${HOST}:${PORT}/${provider}/v1`,
    headers: {
      "x-mock-provider": provider,
      "x-mock-sdk": "@ai-sdk/openai-compatible",
    },
  });
  sdkProviderCache.set(provider, sdkProvider);
  return sdkProvider;
}

function writeJson(res: ServerResponse, status: number, body: unknown): void {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(payload).toString(),
    "access-control-allow-origin": "*",
    "access-control-allow-headers": "content-type, authorization, x-llm-router-account-id",
    "access-control-allow-methods": "GET,POST,OPTIONS",
  });
  res.end(payload);
}

function writeSseHeaders(res: ServerResponse): void {
  res.writeHead(200, {
    "content-type": "text/event-stream; charset=utf-8",
    "cache-control": "no-cache, no-transform",
    connection: "keep-alive",
    "x-accel-buffering": "no",
    "access-control-allow-origin": "*",
  });
}

function sseData(res: ServerResponse, data: unknown): void {
  res.write(`data: ${JSON.stringify(data)}\n\n`);
}

function sseDone(res: ServerResponse): void {
  res.write("data: [DONE]\n\n");
  res.end();
}

async function readJsonBody(req: IncomingMessage): Promise<JsonObject> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  if (chunks.length === 0) {
    return {};
  }
  const raw = Buffer.concat(chunks).toString("utf-8");
  try {
    const parsed = JSON.parse(raw);
    return typeof parsed === "object" && parsed !== null ? (parsed as JsonObject) : {};
  } catch {
    return {};
  }
}

function pathOnly(url: string): string {
  const withoutQuery = url.split("?")[0];
  return withoutQuery.endsWith("/") && withoutQuery !== "/" ? withoutQuery.slice(0, -1) : withoutQuery;
}

function parseEndpoint(url: string): { provider: string; endpoint: EndpointType } | null {
  const clean = pathOnly(url);

  const providerScoped = clean.match(/^\/([^/]+)\/v1\/(chat\/completions|responses)$/);
  if (providerScoped) {
    const endpoint = providerScoped[2] === "responses" ? "responses" : "chat.completions";
    return { provider: providerScoped[1], endpoint };
  }

  const direct = clean.match(/^\/(?:v1\/)?(chat\/completions|responses)$/);
  if (direct) {
    const endpoint = direct[1] === "responses" ? "responses" : "chat.completions";
    return { provider: DEFAULT_PROVIDER, endpoint };
  }

  return null;
}

function pickPrompt(payload: JsonObject): string {
  const input = payload.input;
  if (typeof input === "string" && input.trim()) {
    return input.trim();
  }
  const messages = payload.messages;
  if (Array.isArray(messages)) {
    for (let i = messages.length - 1; i >= 0; i -= 1) {
      const item = messages[i];
      if (typeof item !== "object" || item === null) {
        continue;
      }
      const maybe = (item as JsonObject).content;
      if (typeof maybe === "string" && maybe.trim()) {
        return maybe.trim();
      }
    }
  }
  return "Hello from mock server";
}

function buildAnswer(prompt: string): string {
  return `Mocked answer: ${prompt}`;
}

function isStream(payload: JsonObject): boolean {
  return payload.stream === true;
}

function modelFrom(payload: JsonObject): string {
  return typeof payload.model === "string" && payload.model.trim() ? payload.model : MODEL_FALLBACK;
}

function applyRequestHook(ctx: RequestContext): JsonObject {
  const hook = providerHooks[ctx.provider]?.beforeRequest;
  return hook ? hook(ctx) : ctx.payload;
}

function applyResponseHook(ctx: ResponseContext): string {
  const hook = providerHooks[ctx.provider]?.afterResponse;
  return hook ? hook(ctx) : ctx.answer;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

async function handleChatCompletions(
  req: IncomingMessage,
  res: ServerResponse,
  provider: string,
): Promise<void> {
  const payload = await readJsonBody(req);
  const requestContext: RequestContext = { provider, endpoint: "chat.completions", payload };
  const hookedPayload = applyRequestHook(requestContext);

  const model = modelFrom(hookedPayload);
  const prompt = pickPrompt(hookedPayload);
  const answer = applyResponseHook({
    provider,
    endpoint: "chat.completions",
    payload: hookedPayload,
    model,
    prompt,
    answer: buildAnswer(prompt),
  });

  if (isStream(hookedPayload)) {
    writeSseHeaders(res);
    const id = `chatcmpl_${randomUUID().replaceAll("-", "")}`;

    sseData(res, {
      id,
      object: "chat.completion.chunk",
      created: Math.floor(Date.now() / 1000),
      model,
      choices: [{ index: 0, delta: { role: "assistant" }, finish_reason: null }],
    });

    for (const token of answer.split(" ")) {
      sseData(res, {
        id,
        object: "chat.completion.chunk",
        created: Math.floor(Date.now() / 1000),
        model,
        choices: [{ index: 0, delta: { content: `${token} ` }, finish_reason: null }],
      });
      await delay(STREAM_DELAY_MS);
    }

    sseData(res, {
      id,
      object: "chat.completion.chunk",
      created: Math.floor(Date.now() / 1000),
      model,
      choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
    });
    sseDone(res);
    return;
  }

  writeJson(res, 200, {
    id: `chatcmpl_${randomUUID().replaceAll("-", "")}`,
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [
      {
        index: 0,
        finish_reason: "stop",
        message: {
          role: "assistant",
          content: answer,
        },
      },
    ],
    usage: {
      prompt_tokens: Math.max(1, Math.ceil(prompt.length / 4)),
      completion_tokens: Math.max(1, Math.ceil(answer.length / 4)),
      total_tokens: Math.max(2, Math.ceil((prompt.length + answer.length) / 4)),
    },
    provider,
  });
}

async function handleResponses(req: IncomingMessage, res: ServerResponse, provider: string): Promise<void> {
  const payload = await readJsonBody(req);
  const requestContext: RequestContext = { provider, endpoint: "responses", payload };
  const hookedPayload = applyRequestHook(requestContext);

  const model = modelFrom(hookedPayload);
  const prompt = pickPrompt(hookedPayload);
  const outputText = applyResponseHook({
    provider,
    endpoint: "responses",
    payload: hookedPayload,
    model,
    prompt,
    answer: buildAnswer(prompt),
  });
  const id = `resp_${randomUUID().replaceAll("-", "")}`;

  if (isStream(hookedPayload)) {
    writeSseHeaders(res);

    sseData(res, {
      type: "response.created",
      response: {
        id,
        object: "response",
        created_at: Math.floor(Date.now() / 1000),
        model,
        status: "in_progress",
      },
    });

    for (const token of outputText.split(" ")) {
      sseData(res, { type: "response.output_text.delta", delta: `${token} ` });
      await delay(STREAM_DELAY_MS);
    }

    sseData(res, {
      type: "response.completed",
      response: {
        id,
        object: "response",
        created_at: Math.floor(Date.now() / 1000),
        model,
        status: "completed",
        output: [
          {
            type: "message",
            role: "assistant",
            content: [{ type: "output_text", text: outputText }],
          },
        ],
      },
    });

    sseDone(res);
    return;
  }

  writeJson(res, 200, {
    id,
    object: "response",
    created_at: Math.floor(Date.now() / 1000),
    model,
    status: "completed",
    output: [
      {
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text: outputText }],
      },
    ],
    output_text: outputText,
    usage: {
      input_tokens: Math.max(1, Math.ceil(prompt.length / 4)),
      output_tokens: Math.max(1, Math.ceil(outputText.length / 4)),
      total_tokens: Math.max(2, Math.ceil((prompt.length + outputText.length) / 4)),
    },
    provider,
  });
}

function shutdown(res: ServerResponse, server: ReturnType<typeof createServer>): void {
  writeJson(res, 200, { ok: true, message: "Shutting down" });
  setTimeout(() => {
    server.close(() => {
      process.exit(0);
    });
    setTimeout(() => {
      process.exit(0);
    }, 500).unref();
  }, 15).unref();
}

const server = createServer(async (req, res) => {
  const method = req.method ?? "GET";
  const url = req.url ?? "/";
  const cleanPath = pathOnly(url);

  if (method === "OPTIONS") {
    res.writeHead(204, {
      "access-control-allow-origin": "*",
      "access-control-allow-headers": "content-type, authorization, x-llm-router-account-id",
      "access-control-allow-methods": "GET,POST,OPTIONS",
    });
    res.end();
    return;
  }

  if (method === "GET" && cleanPath === "/health") {
    writeJson(res, 200, {
      ok: true,
      service: "llm-router-mock",
      uptime_sec: process.uptime(),
      default_provider: DEFAULT_PROVIDER,
      configured_hook_providers: Object.keys(providerHooks),
    });
    return;
  }

  if ((method === "POST" || method === "GET") && cleanPath === "/shutdown") {
    shutdown(res, server);
    return;
  }

  if (method === "POST") {
    const endpoint = parseEndpoint(cleanPath);
    if (endpoint) {
      // Initialize per-provider @ai-sdk openai-compatible clients lazily.
      getSdkProvider(endpoint.provider);

      if (endpoint.endpoint === "chat.completions") {
        await handleChatCompletions(req, res, endpoint.provider);
        return;
      }
      await handleResponses(req, res, endpoint.provider);
      return;
    }
  }

  writeJson(res, 404, {
    error: {
      message: `No mock route for ${method} ${cleanPath}`,
      type: "invalid_request_error",
    },
  });
});

server.listen(PORT, HOST, () => {
  // eslint-disable-next-line no-console
  console.log(`[mock-server] listening on http://${HOST}:${PORT}`);
  // eslint-disable-next-line no-console
  console.log(`[mock-server] default provider: ${DEFAULT_PROVIDER}`);
  // eslint-disable-next-line no-console
  console.log("[mock-server] routes: /:provider/v1/chat/completions, /:provider/v1/responses, /shutdown");
});

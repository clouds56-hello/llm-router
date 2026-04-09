# llm-router (Rust)

`llm-router` is an OpenAI-compatible gateway that routes requests to multiple upstream LLM providers (OpenAI, Claude, Copilot, Codex) with failover, circuit-breaking, pluggable logging, and a built-in viewer.

## Implemented v1 Endpoints

- `POST /v1/chat/completions` (non-stream + SSE stream)
- `POST /v1/embeddings`
- `GET /v1/models`
- `GET /healthz`
- `GET /viewer` dashboard (when enabled)

## Features

- OpenAI-compatible wire format at the router edge.
- Adapter abstraction per provider.
- Route policy with ordered failover chain + retry budget + circuit-open backoff.
- YAML config (`router.yaml`) with env-secret resolution and file hot reload.
- In-process plugin pipeline with bounded queue.
- Default plugins:
  - Structured JSON logger
  - Metrics counters
  - SQLite event sink
- Built-in viewer using SQLite-backed event history and stats.
- Retention pruning by max age and max rows.

## Quick Start

1. Set API key env vars referenced by `router.yaml`:

```bash
export OPENAI_API_KEY=...
export ANTHROPIC_API_KEY=...
export COPILOT_API_KEY=...
export CODEX_API_KEY=...
```

2. Start the server:

```bash
cargo run -p llm-routerd -- router.yaml
```

3. Call OpenAI-compatible endpoint:

```bash
curl -s http://127.0.0.1:8787/v1/models
```

4. Open viewer:

- `http://127.0.0.1:8787/viewer`

## Workspace Layout

- `router-core`: config, routing engine, protocol models, shared errors
- `adapter-openai`: OpenAI adapter
- `adapter-claude`: Claude adapter with protocol translation
- `adapter-copilot`: Copilot adapter
- `adapter-codex`: Codex adapter
- `plugin-api`: plugin trait + queue manager
- `plugin-defaults`: logger/metrics/sqlite sink + storage queries
- `viewer-web`: embedded dashboard routes/UI
- `llm-routerd`: executable server, API handlers, hot reload wiring

## Notes

- Streaming currently uses normalized SSE output; upstream true token-stream pass-through can be extended per adapter.
- Claude embeddings are explicitly unsupported in v1 and return an OpenAI-style request error.

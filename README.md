# llm-router

Desktop app with a local OpenAI-compatible router.

Stack:
- Tauri (desktop shell)
- Vite + React (dashboard)
- Rust (`tokio`, `axum`, `reqwest`, `serde`, `tracing`)

## Features

Core API endpoints:
- `POST /v1/chat/completions`
- `POST /v1/responses`

Routing providers:
- `openai` (implemented)
- `deepseek` (stub adapter)
- `claude` (stub adapter)
- `github-copilot` (stub adapter + request-decoration scaffold)

Streaming:
- SSE supported via `stream: true` on chat endpoint.

Desktop behavior:
- Tauri process starts and manages the embedded local router (`127.0.0.1:11434` by default).

Dashboard includes:
- provider status
- model list
- credential account manager (connect/disconnect/default/rename/enable)
- active config inspection
- request logs
- login status
- streaming test console

Copilot auth:
- OAuth device flow scaffold for GitHub.com + Enterprise
- enterprise domain normalization
- API base derivation helper
- OAuth token persistence in `credentials.yaml` provider accounts
- login/logout/status Tauri commands
- dedicated adapter scaffold with TODOs for provider-specific request behavior

Config system:
- YAML split into:
  - `providers.yaml`
  - `models.yaml`
  - `credentials.yaml`
- `credentials.yaml` uses provider account collections:
  - `providers.<provider>.accounts[]`
  - account fields: `id`, `label`, `auth_type`, `is_default`, `enabled`, `secrets`, `meta`
- inline secret codec:
  - `enc2:<version>.<algo>.<nonce>.<payload>`
  - self-contained obfuscation only (not cryptographic security)
- hot reload watcher (no restart required)
- validation + reload errors exposed to API/UI logs

## Project layout

- `core/src/router`: axum API router and handlers
- `core/src/providers`: provider trait + adapters
- `core/src/config`: YAML loading/hot-reload/validation
- `core/src/auth`: Copilot OAuth device-flow manager
- `core/src/logging`: pluggable log sink trait + in-memory viewer sink
- `app/src-tauri/src/tauri_api.rs`: Tauri command bridge for frontend
- `app/src`: React dashboard
- `app/src-tauri/config`: default runtime YAML files
- `examples`: example YAML files

## Example config files

See:
- `examples/providers.yaml`
- `examples/models.yaml`
- `examples/credentials.yaml`

## Curl examples

Chat:

```bash
curl -s http://127.0.0.1:11434/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-4.1-mini",
    "messages": [{"role":"user","content":"Hello"}]
  }'
```

Streaming chat (SSE):

```bash
curl -N http://127.0.0.1:11434/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-4.1-mini",
    "stream": true,
    "messages": [{"role":"user","content":"Stream a short answer"}]
  }'
```

Responses:

```bash
curl -s http://127.0.0.1:11434/v1/responses \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-4.1-mini",
    "input": "Summarize async Rust in one sentence"
  }'
```

## Run locally

Prerequisites:
- Rust toolchain
- Node.js 18+
- Tauri system prerequisites (WebView + platform SDKs)

Install frontend dependencies:

```bash
pnpm install
```

Run frontend only:

```bash
pnpm --filter ./app run dev
```

Run Rust tests:

```bash
cargo test -p llm-router-core
```

Launch Tauri app (starts router + dashboard):

```bash
pnpm --filter ./app run tauri dev
```

Build desktop app:

```bash
pnpm --filter ./app run tauri build
```

## Test coverage

Implemented tests include:
- routing behavior (`chat` and `responses`)
- SSE chat streaming path
- config hot reload
- Copilot auth/config plumbing (domain normalization + API base derivation)

## Notes

- OpenAI adapter is the first fully working provider path.
- DeepSeek/Claude/Copilot adapters intentionally compile as stubs with TODO markers where provider behavior is uncertain.
- Copilot Enterprise API URL derivation can vary by org setup; adapter/auth code is structured for follow-up refinements.

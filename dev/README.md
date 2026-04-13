# dev mock server

TypeScript OpenAI-compatible mock upstream for local llm-router development, built with `@ai-sdk/openai-compatible`.

## run

```bash
pnpm --dir dev install
pnpm --dir dev run mock:server
```

Optional env vars:

- `MOCK_HOST` (default `127.0.0.1`)
- `MOCK_PORT` (default `4010`)
- `MOCK_MODEL` (default `gpt-mock-1`)
- `MOCK_DEFAULT_PROVIDER` (default `openai`)
- `MOCK_STREAM_DELAY_MS` (default `35`)

## endpoints

- `GET /health`
- `POST /<provider>/v1/chat/completions`
- `POST /<provider>/v1/responses`
- `POST /v1/chat/completions` (uses `MOCK_DEFAULT_PROVIDER`)
- `POST /v1/responses` (uses `MOCK_DEFAULT_PROVIDER`)
- `POST /chat/completions` (codex-style path, uses `MOCK_DEFAULT_PROVIDER`)
- `POST /responses` (codex-style path, uses `MOCK_DEFAULT_PROVIDER`)
- `POST /shutdown` (or `GET /shutdown`) to stop the server process

Both chat and responses endpoints support `stream: true` SSE.

## provider hooks

Mock hooks are applied per provider:

- `openai`: normalizes model and prefixes output with `[openai]`
- `anthropic`: adds `metadata.provider_hint` and prefixes output with `[anthropic]`
- `deepseek`: defaults `temperature` to `0.2` and prefixes output with `[deepseek]`

## sample provider config

```yaml
providers:
  mock_openai:
    provider_type: openai-compatible
    base_url: http://127.0.0.1:4010/openai
    enabled: true
```

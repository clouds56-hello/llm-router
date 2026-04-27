# llm-router

Turn a GitHub Copilot subscription into a local **OpenAI-compatible** API.

Single static binary, written in Rust. No database, no web UI, no third-party
services. Just a focused CLI that:

- Logs into GitHub via device flow (or imports an existing token from `gh` /
  the official Copilot plugin).
- Exposes `POST /v1/chat/completions` and `GET /v1/models` on `127.0.0.1`.
- Forwards requests to `api.githubcopilot.com` with full SSE streaming.
- Pools multiple Copilot accounts with round-robin and automatic failover.
- Records per-request usage to a local SQLite file.

Inspired by [`sub2api`](https://github.com/Wei-Shaw/sub2api) but intentionally
minimal.

## Install

```sh
cargo install --path .
```

## Quick start

```sh
# 1. Add a Copilot account (GitHub device flow)
llm-router login

#    or import an existing GitHub token
llm-router import --from gh

# 2. Start the local server (default 127.0.0.1:4141)
llm-router serve

# 3. Point any OpenAI-compatible client at it
curl http://127.0.0.1:4141/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role":"user","content":"hi"}],
    "stream": true
  }'
```

## Config

`~/.config/llm-router/config.toml` (mode `0600`).

```toml
[server]
host = "127.0.0.1"
port = 4141

[pool]
strategy = "round_robin"
failure_cooldown_secs = 60

[usage]
enabled = true

[copilot]
editor_version         = "vscode/1.95.0"
editor_plugin_version  = "copilot-chat/0.20.0"
user_agent             = "GitHubCopilotChat/0.20.0"
copilot_integration_id = "vscode-chat"
openai_intent          = "conversation-panel"

[[accounts]]
id = "personal"
github_token = "gho_..."
```

If Copilot starts rejecting requests with 401/403 + HTML, your editor identity
headers are likely stale. Bump `[copilot]` to match the current VS Code Copilot
Chat extension and restart.

## Commands

```
llm-router login                    # GitHub device flow
llm-router import --from gh         # or --from copilot-plugin
llm-router account list|remove ID|show ID
llm-router headers [--account ID]   # inspect resolved Copilot identity headers
llm-router serve [--port N]
llm-router usage [--since 24h] [--account ID]
```

## License

Dual-licensed under MIT or Apache-2.0.

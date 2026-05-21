# tokn-router

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
tokn-router login

#    or import an existing GitHub token
tokn-router import --from gh

# 2. Start the local server (default 127.0.0.1:4141)
tokn-router serve

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

`~/.config/tokn-router/config.toml` (mode `0600`).

```toml
[server]
host = "127.0.0.1"
port = 4141

[pool]
strategy = "round_robin"
failure_cooldown_secs = 60

[usage]
enabled = true

# Optional outbound HTTP/HTTPS/SOCKS5 proxy. Omit the section entirely to make
# a direct connection. Setting `system = true` defers to the standard
# HTTP_PROXY / HTTPS_PROXY env vars.
[proxy]
# url = "http://user:pass@proxy.example.com:8080"
# url = "socks5h://127.0.0.1:1080"
# system = false
# no_proxy = ["localhost", "127.0.0.1", ".internal"]

[[accounts]]
id = "personal"
provider = "github-copilot"   # default — see "Providers" below for others
auth_type = "bearer"
refresh_token = "gho_..."
access_token = "tid=..."
access_token_expires_at = 1730000000
refresh_url = "https://api.github.com/copilot_internal/v2/token"

[accounts.settings]
editor_version         = "vscode/1.95.0"
editor_plugin_version  = "copilot-chat/0.20.0"
user_agent             = "GitHubCopilotChat/0.20.0"
copilot_integration_id = "vscode-chat"
openai_intent          = "conversation-panel"
initiator_mode         = "auto"
```

The downstream client may also send `X-Initiator: user|agent` per request,
which overrides the auto-classifier and the config setting.

If Copilot starts rejecting requests with 401/403 + HTML, your editor identity
headers are likely stale. Bump the account's `[accounts.settings]` values to
match the current VS Code Copilot Chat extension and restart.

## Commands

```
tokn-router login [--provider PROVIDER] [--no-proxy]
tokn-router import --from gh|copilot-plugin|env [--provider PROVIDER] [--env-var NAME]
tokn-router account list|remove ID|show ID
tokn-router headers [--account ID]   # inspect resolved Copilot identity headers
tokn-router serve [--port N] [--with-proxy] [--proxy-route-mode MODE] [--no-proxy] [--allow-remote]
tokn-router proxy [start] [--port N] [--route-mode MODE] [--no-proxy] [--allow-remote]
tokn-router proxy env [--shell sh|fish|pwsh]
tokn-router proxy shell [--shell /path/to/shell]
tokn-router proxy ca path|show|regenerate
tokn-router usage [--since 24h] [--account ID]
tokn-router config get|set|unset KEY [--account ID] [--add]
tokn-router config list | edit | path
```

## Proxy Mode

`proxy` runs a local HTTP CONNECT forward proxy that MITMs a small allowlist of
LLM API hosts and routes those requests through tokn-router's existing account
pool.

First run generates a local CA under `~/.config/tokn-router/ca/`:

```sh
tokn-router proxy
tokn-router proxy ca show
```

Then trust the printed CA cert and inject proxy + CA env vars into your shell:

```sh
eval "$(tokn-router proxy env)"
```

Or spawn a subshell with those variables already set:

```sh
tokn-router proxy shell
```

`proxy shell` uses `SHELL` when available and falls back to `/bin/sh`. Pass
`--shell /path/to/shell` to override detection.

The emitted env block sets:

- `HTTPS_PROXY` / `HTTP_PROXY`
- `SSL_CERT_FILE` (to a generated merged bundle containing system roots + the tokn-router CA)
- `NODE_EXTRA_CA_CERTS`
- `REQUESTS_CA_BUNDLE` (merged bundle)
- `CURL_CA_BUNDLE` (merged bundle)
- `GIT_SSL_CAINFO` (merged bundle)

`NODE_EXTRA_CA_CERTS` still points at the local `ca.crt` because Node appends it to
the built-in trust store. The other variables point at `ca-bundle.crt` so they do
not drop system roots.

Config:

```toml
[proxy_mode]
host = "127.0.0.1"
port = 4142
route_mode = "route"

# optional; defaults to ~/.config/tokn-router/ca
# ca_dir = "/some/path"

# extend the built-in MITM allowlist
# intercept_hosts = ["my-tokn-gateway.example.com"]

# force selected hosts to pass through untouched
# passthrough_hosts = ["api.githubcopilot.com"]
```

Requests to hosts outside the allowlist are tunneled through untouched.

`tokn-router serve --with-proxy` runs the OpenAI-compatible HTTP server and the
MITM proxy together in one process. They share the same account pool and event
pipeline, but each listener can keep its own default route mode via
`[server].route_mode` and `[proxy_mode].route_mode` (or `--proxy-route-mode`).

## Providers

| id | auth | notes |
|---|---|---|
| `github-copilot` (default) | GitHub OAuth device flow → short-lived API token | identity headers; auto-classified `X-Initiator` |
| `zai-coding-plan` / `zai` / `zhipuai-coding-plan` / `zhipuai` | static API key (`Authorization: Bearer ...`) | Four provider ids sharing one implementation; OpenAI-compatible upstream; auto-injects `thinking: { type: "enabled", clear_thinking: false }` for reasoning models |

The four Z.ai provider ids share one backend implementation, but each id is
registered independently. The default upstream is
`https://api.z.ai/api/coding/paas/v4`; override per-account for the
China-mainland endpoint:

```toml
[[accounts]]
id = "coding-plan"
provider = "zai-coding-plan"
auth_type = "bearer"
api_key = "sk-..."
base_url = "https://open.bigmodel.cn/api/paas/v4"
```

Add a Z.ai account interactively (key is read with hidden input and verified
against `/models`):

```sh
tokn-router login --provider zai-coding-plan
# or non-interactively from the environment
ZAI_API_KEY=sk-... tokn-router import --from env --provider zai --id work
```

`/v1/models` returns the upstream OpenAI-shape entries unchanged; each entry
gains an `x_tokn_router` block with the resolved provider id, display name,
auth kind, and (when known) static capabilities/cost/limit metadata.

## License

Dual-licensed under MIT or Apache-2.0.

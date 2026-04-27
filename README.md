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
# How to populate the `X-Initiator` header on outbound chat requests.
# - "auto"         (default) classify per request: "user" for a fresh human turn,
#                  "agent" for tool-result follow-ups. Avoids being billed once
#                  per tool round-trip on Copilot's premium models.
# - "always_user"
# - "always_agent"
initiator_mode = "auto"

# Optional persona overlay. When set, header values from the matching profile
# in `profiles.toml` are merged in *before* the explicit fields above (so
# anything you pin in `[copilot]` still wins). The downstream client may also
# send `X-Behave-As: <persona>` per request, which overrides this.
# behave_as = "opencode"

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
provider = "github-copilot"   # default; only one provider supported in v1
github_token = "gho_..."
```

The downstream client may also send `X-Initiator: user|agent` per request,
which overrides the auto-classifier and the config setting.

If Copilot starts rejecting requests with 401/403 + HTML, your editor identity
headers are likely stale. Bump `[copilot]` to match the current VS Code Copilot
Chat extension and restart.

## Commands

```
llm-router login [--no-proxy]       # GitHub device flow
llm-router import --from gh         # or --from copilot-plugin
llm-router account list|remove ID|show ID
llm-router headers [--account ID]   # inspect resolved Copilot identity headers
llm-router serve [--port N] [--no-proxy] [--allow-remote]
llm-router usage [--since 24h] [--account ID]
llm-router config get|set|unset KEY [--account ID] [--add]
llm-router config list | edit | edit-profiles | path [--profiles] | list-profiles
```

### Personas (`behave_as`)

Profiles live in an embedded `profiles.toml`; you can extend or override them
with `~/.config/llm-router/profiles.toml` (`llm-router config edit-profiles`).
Schema:

```toml
[<persona>.<scope>]
verified = true|false       # if false, llm-router warns on use
editor-version         = "..."   # wire-form (kebab-case) header names
editor-plugin-version  = "..."
user-agent             = "..."
copilot-integration-id = "..."
openai-intent          = "..."
```

`<scope>` is `default` (always merged), an upstream id like `github-copilot`
(merged when sending to that upstream), or `general` (fallback when no
upstream-specific scope matches).

Built-in personas: `copilot` (verified), `opencode`, `codex`, `openclaw`
(placeholders — PRs welcome with verified header values).

Selection precedence (low → high):
compile-time defaults → `[copilot] behave_as` → `[[accounts]] behave_as` →
inbound `X-Behave-As` → user-explicit `[copilot]` fields →
per-account `[copilot]` overrides.

## License

Dual-licensed under MIT or Apache-2.0.

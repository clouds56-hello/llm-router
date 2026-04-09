Build a production-grade Rust project named `llm-router` from scratch.

Goal:
Create an OpenAI-compatible API gateway that routes requests to multiple providers (`openai`, `copilot`, `codex`, `claude`) behind one unified OpenAI-style interface.

Core requirements:
1) OpenAI-compatible endpoints
- Implement:
  - `POST /v1/chat/completions` (streaming + non-streaming)
  - `POST /v1/embeddings`
- Request/response schemas should be OpenAI-compatible enough for existing OpenAI SDKs to work.

2) Multi-provider routing
- Add provider adapters for OpenAI, Copilot, Codex, Claude.
- Normalize each provider into a common internal trait/interface.
- Route by model name/prefix and configurable rules.

3) SSE end-to-end
- Support true streaming for chat completions via Server-Sent Events.
- Preserve chunked deltas and finish semantics.
- Include backpressure-safe async streaming pipeline.

4) Config + secrets in YAML with hot reload
- Store provider credentials and routing rules in YAML files.
- Use file-watch hot reload without process restart.
- Validate config on reload and keep last known good config on failure.

5) Pluggable logger/viewer (in-process Rust traits)
- Define Rust traits for logging/event sinks and viewer backends.
- Provide default implementations.
- Make plugin points explicit and documented.

6) Built-in web dashboard
- Serve a built-in web UI from the same binary.
- Show providers, models, route rules, health, request metrics, recent logs, and active streams.
- No external DB required for MVP.

7) Non-functional requirements
- Async Rust (`tokio`), strong error handling (`thiserror`/`anyhow`), structured logging (`tracing`).
- Modular crate layout.
- Configurable timeouts/retries/circuit-breaker basics.
- Basic auth or API-key protection for admin dashboard.
- CORS support for API.

8) Testing
- Unit tests for routing/config parsing/hot-reload behavior.
- Integration tests for `/v1/chat/completions` and `/v1/embeddings`.
- Streaming test for SSE.

9) Developer experience
- Provide:
  - `Cargo.toml`
  - complete source tree
  - sample `config.yaml`
  - `.env.example`
  - `README.md` with quick start and curl examples
  - dashboard screenshot placeholder instructions
- Include `docker-compose.yml` for local run.
- Include `Makefile` or task runner commands.

Implementation guidance:
- Prefer `axum` + `tokio` + `serde` + `reqwest`.
- Use trait-based provider abstraction:
  - `ChatProvider`
  - `EmbeddingProvider`
  - `LoggerPlugin`
  - `ViewerPlugin`
- Keep API compatibility strict at boundaries, and adapt provider-specific differences internally.

Deliverable format:
- Output full project files with paths and contents.
- Ensure code compiles.
- Include clear run instructions:
  - `cargo build`
  - `cargo test`
  - `cargo run`
- Include 3 curl demos:
  - non-stream chat
  - SSE stream chat
  - embeddings

If any provider-specific API details are uncertain, create clean adapter stubs with TODO markers and keep the system fully runnable with at least one functional provider path.

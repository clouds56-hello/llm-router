Create a desktop app named `llm-router` using:

- Tauri
- Vite
- React
- Rust backend

What it should do:

Core API
- Expose OpenAI-compatible APIs from the Rust side:
  - `POST /v1/chat/completions`
  - `POST /v1/responses`
- Route requests to these providers:
  - openai
  - deepseek
  - claude
  - github-copilot
- Support streaming chat via SSE.
- Ensure the desktop app can start and manage the local router server.

Desktop app
- Build a Tauri desktop app with:
  - a Rust backend service for routing
  - a Vite + React frontend for the UI
- Include a built-in dashboard in the React app for:
  - provider status
  - model list
  - active config inspection
  - request logs / viewer
  - login status
  - streaming test console

GitHub Copilot login
- Implement GitHub Copilot login based on the opencode GitHub Copilot plugin approach:
  - use OAuth device authorization flow
  - support both GitHub.com and GitHub Enterprise
  - allow user to choose deployment type:
    - GitHub.com
    - GitHub Enterprise
  - if enterprise is selected, accept an enterprise URL/domain
  - normalize enterprise domain and derive Copilot API base URL accordingly
  - persist Copilot auth state securely on device
  - expose login / logout / auth status in the UI
- Keep implementation practical:
  - if some Copilot-specific behavior is unclear, implement it behind a dedicated adapter with TODOs
  - include the auth flow scaffolding and one working end-to-end path where feasible
- For Copilot adapter behavior, structure the code so request decoration can support provider-specific headers and request classification needed by Copilot-compatible traffic.
- See https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/plugin/github-copilot/copilot.ts

Configuration
- Split config into three YAML files:
  - `providers.yaml`
    - provider definitions
    - base URLs
    - routing metadata
    - enabled/disabled flags
  - `models.yaml`
    - model catalog
    - provider-to-model mapping
    - OpenAI-compatible exposed model names
    - default routing rules
  - `credentials.yaml`
    - provider credentials and auth-related settings
- Support hot reload for all YAML config files without restart.
- Validate configs and surface parse/reload errors in the UI and logs.

Architecture
- Use Rust traits for pluggable provider adapters.
- Use Rust traits for pluggable logger/viewer integrations.
- Keep provider adapters isolated so new providers can be added later.
- Make OpenAI the first fully working adapter.
- Implement stub adapters with TODOs where provider-specific details are uncertain, but keep the project compiling.
- Organize the project so the Tauri shell, router core, provider adapters, config loader, auth manager, and frontend are clearly separated.

Tech choices
- Rust
- tokio
- axum
- serde
- reqwest
- tracing
- tauri
- vite
- react

Output requirements
- Produce a complete project scaffold and implementation.
- Include basic tests for:
  - routing
  - chat endpoint
  - response endpoint
  - SSE streaming path
  - config hot reload
  - GitHub Copilot auth/config plumbing where testable
- Include example YAML files for:
  - `providers.yaml`
  - `models.yaml`
  - `credentials.yaml`
- Include curl examples for:
  - chat
  - streaming chat
  - response
- Include brief run instructions for:
  - local development
  - launching the Tauri app

Keep assumptions minimal
- If provider-specific details are unclear, implement stub adapters with TODOs.
- Ensure the project still compiles and runs with at least one working provider adapter.
- Prefer clear interfaces and compile-safe placeholders over fake provider behavior.

References
- https://github.com/vercel/ai/blob/main/packages/openai-compatible/src/chat/openai-compatible-chat-language-model.ts
- https://github.com/vercel/ai/blob/main/packages/open-responses/src/open-responses-provider.ts
- https://github.com/vercel/ai/blob/main/packages/deepseek/src/deepseek-provider.ts
- https://github.com/vercel/ai/blob/main/packages/anthropic/src/anthropic-provider.ts

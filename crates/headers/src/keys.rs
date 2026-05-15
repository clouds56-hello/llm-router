//! Catalogue of popular header names as compile-time `static` constants.
//!
//! Use these in place of ad-hoc `HeaderName::new(...)` for any header that
//! appears in this list — both for clarity and to avoid the (small) cost of
//! re-allocating the `SmolStr` on each construction.

use crate::name::HeaderName;

macro_rules! key {
  ($name:ident, $original:literal, $lower:literal) => {
    pub static $name: HeaderName = HeaderName::new_static($original, $lower);
  };
}

// Core HTTP transport
key!(AUTHORIZATION, "Authorization", "authorization");
key!(CONTENT_TYPE, "Content-Type", "content-type");
key!(CONTENT_ENCODING, "Content-Encoding", "content-encoding");
key!(CONTENT_LENGTH, "Content-Length", "content-length");
key!(ACCEPT, "Accept", "accept");
key!(ACCEPT_ENCODING, "Accept-Encoding", "accept-encoding");
key!(ACCEPT_LANGUAGE, "Accept-Language", "accept-language");
key!(CONNECTION, "Connection", "connection");
key!(COOKIE, "Cookie", "cookie");
key!(USER_AGENT, "User-Agent", "user-agent");
key!(HOST, "Host", "host");

// Tool / persona identity
key!(EDITOR_VERSION, "Editor-Version", "editor-version");
key!(EDITOR_PLUGIN_VERSION, "Editor-Plugin-Version", "editor-plugin-version");
key!(COPILOT_INTEGRATION_ID, "Copilot-Integration-Id", "copilot-integration-id");
key!(COPILOT_VISION_REQUEST, "Copilot-Vision-Request", "copilot-vision-request");
key!(OPENAI_INTENT, "OpenAI-Intent", "openai-intent");
key!(OPENAI_BETA, "OpenAI-Beta", "openai-beta");
key!(CHATGPT_ACCOUNT_ID, "chatgpt-account-id", "chatgpt-account-id");
key!(ANTHROPIC_BETA, "Anthropic-Beta", "anthropic-beta");
key!(ANTHROPIC_VERSION, "Anthropic-Version", "anthropic-version");
key!(X_API_KEY, "X-Api-Key", "x-api-key");

// Router-injected correlation
key!(X_SESSION_ID, "X-Session-Id", "x-session-id");
key!(X_SESSION_AFFINITY, "X-Session-Affinity", "x-session-affinity");
key!(X_PARENT_SESSION_ID, "X-Parent-Session-Id", "x-parent-session-id");
key!(X_REQUEST_ID, "X-Request-Id", "x-request-id");
key!(X_INITIATOR, "X-Initiator", "x-initiator");
key!(X_PROJECT_CWD, "X-Project-Cwd", "x-project-cwd");
key!(X_INTERACTION_ID, "X-Interaction-Id", "x-interaction-id");
key!(X_BEHAVE_AS, "X-Behave-As", "x-behave-as");

// Codex CLI native (lowercase, no x- prefix in real captures)
key!(ORIGINATOR, "originator", "originator");
key!(VERSION, "version", "version");
key!(SESSION_ID_LOWER, "session_id", "session_id");
key!(THREAD_ID, "thread_id", "thread_id");
key!(X_CLIENT_REQUEST_ID, "x-client-request-id", "x-client-request-id");
key!(X_CODEX_BETA_FEATURES, "x-codex-beta-features", "x-codex-beta-features");
key!(X_CODEX_TURN_METADATA, "x-codex-turn-metadata", "x-codex-turn-metadata");
key!(X_CODEX_WINDOW_ID, "x-codex-window-id", "x-codex-window-id");

#[cfg(test)]
mod tests {
  use super::*;

  /// Sanity test: every catalogued key's original form must lowercase to its
  /// declared lowercase form. Catches typos in `key!()` macro calls.
  #[test]
  fn original_lowercases_to_canonical() {
    macro_rules! check {
      ($($name:ident),* $(,)?) => {
        $({
          let n = &$name;
          assert_eq!(
            n.original().to_ascii_lowercase(),
            n.as_str(),
            "key {} declared lower form does not match", stringify!($name)
          );
        })*
      };
    }
    check!(
      AUTHORIZATION, CONTENT_TYPE, CONTENT_ENCODING, CONTENT_LENGTH, ACCEPT, ACCEPT_ENCODING,
      ACCEPT_LANGUAGE, CONNECTION, COOKIE, USER_AGENT, HOST, EDITOR_VERSION,
      EDITOR_PLUGIN_VERSION, COPILOT_INTEGRATION_ID, COPILOT_VISION_REQUEST, OPENAI_INTENT,
      OPENAI_BETA, CHATGPT_ACCOUNT_ID, ANTHROPIC_BETA, ANTHROPIC_VERSION, X_API_KEY,
      X_SESSION_ID, X_SESSION_AFFINITY, X_PARENT_SESSION_ID, X_REQUEST_ID, X_INITIATOR,
      X_PROJECT_CWD, X_INTERACTION_ID, X_BEHAVE_AS, ORIGINATOR, VERSION, SESSION_ID_LOWER,
      THREAD_ID, X_CLIENT_REQUEST_ID, X_CODEX_BETA_FEATURES, X_CODEX_TURN_METADATA,
      X_CODEX_WINDOW_ID,
    );
  }

  #[test]
  fn keys_are_case_insensitive_to_arbitrary_input() {
    use crate::HeaderName;
    assert_eq!(AUTHORIZATION, HeaderName::new("AUTHORIZATION"));
    assert_eq!(EDITOR_VERSION, HeaderName::new("editor-version"));
  }
}

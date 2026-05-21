//! Registry mapping `(provider, client_id)` to the typed header schema pair.
//!
//! Each successful lookup yields a [`ResolvedSchema`] describing which header
//! struct and (optional) overlay struct should be applied. Composition is
//! overlay-wins via [`HeaderMap::merge_replacing`]: any name set by both the
//! client-id headers and the overlay takes the overlay's value.
//!
//! Unknown personas for a known provider fall back to [`PersonaKind::Opencode`]
//! as a sensible default base. Unknown providers return [`None`].

use crate::map::HeaderMap;
use crate::persona::Persona;

/// Closed enum of personas with a typed header schema. Personas outside this
/// set (i.e. [`Persona::Custom`]) fall back to [`PersonaKind::Opencode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PersonaKind {
  Opencode,
  CodexCli,
  ClaudeCode,
  Cline,
  CopilotCli,
}

impl PersonaKind {
  /// Map a [`Persona`] to its [`PersonaKind`], or `None` for `Custom`.
  pub fn from_persona(p: &Persona) -> Option<Self> {
    match p {
      Persona::Opencode => Some(Self::Opencode),
      Persona::CodexCli => Some(Self::CodexCli),
      Persona::ClaudeCode => Some(Self::ClaudeCode),
      Persona::Cline => Some(Self::Cline),
      Persona::CopilotCli => Some(Self::CopilotCli),
      Persona::Custom(_) => None,
    }
  }
}

/// Closed enum of provider transport overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverlayKind {
  Copilot,
  Codex,
}

/// The schema pair selected for a given `(provider, persona)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSchema {
  pub persona: PersonaKind,
  pub overlay: Option<OverlayKind>,
}

impl ResolvedSchema {
  /// Compose persona-built and overlay-built maps into a single map. Overlay
  /// values win on name conflict; otherwise persona ordering is preserved.
  pub fn compose(persona_map: HeaderMap, overlay_map: Option<HeaderMap>) -> HeaderMap {
    let mut out = persona_map;
    if let Some(o) = overlay_map {
      out.merge_replacing(o);
    }
    out
  }
}

/// Look up the schema pair for a given `(provider, client_id)`. Returns `None`
/// for unknown providers; for known providers, falls back to
/// [`PersonaKind::Opencode`] as the base persona when the input persona is
/// [`Persona::Custom`].
pub fn lookup(provider: &str, persona: &Persona) -> Option<ResolvedSchema> {
  let base = PersonaKind::from_persona(persona).unwrap_or(PersonaKind::Opencode);
  match provider {
    "openai" => Some(ResolvedSchema {
      persona: base,
      overlay: matches!(base, PersonaKind::CodexCli).then_some(OverlayKind::Codex),
    }),
    "copilot" | "github-copilot" => Some(ResolvedSchema {
      persona: base,
      overlay: Some(OverlayKind::Copilot),
    }),
    "deepseek" | "zai" | "zai-coding-plan" | "zhipuai" | "zhipuai-coding-plan" | "codex" => Some(ResolvedSchema {
      persona: base,
      overlay: matches!(provider, "codex").then_some(OverlayKind::Codex),
    }),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::keys;
  use crate::name::HeaderName;
  use crate::value::HeaderValue;

  #[test]
  fn lookup_known_pairs() {
    assert_eq!(
      lookup("openai", &Persona::CodexCli),
      Some(ResolvedSchema {
        persona: PersonaKind::CodexCli,
        overlay: Some(OverlayKind::Codex)
      })
    );
    assert_eq!(
      lookup("copilot", &Persona::Opencode),
      Some(ResolvedSchema {
        persona: PersonaKind::Opencode,
        overlay: Some(OverlayKind::Copilot)
      })
    );
    assert_eq!(
      lookup("deepseek", &Persona::ClaudeCode),
      Some(ResolvedSchema {
        persona: PersonaKind::ClaudeCode,
        overlay: None
      })
    );
  }

  #[test]
  fn unknown_persona_falls_back_to_opencode() {
    let custom = Persona::from_str_lossy("my-tool");
    let r = lookup("copilot", &custom).unwrap();
    assert_eq!(r.persona, PersonaKind::Opencode);
    assert_eq!(r.overlay, Some(OverlayKind::Copilot));
  }

  #[test]
  fn unknown_provider_returns_none() {
    assert!(lookup("nonesuch", &Persona::Opencode).is_none());
  }

  #[test]
  fn openai_with_non_codex_persona_has_no_overlay() {
    let r = lookup("openai", &Persona::Opencode).unwrap();
    assert!(r.overlay.is_none());
  }

  #[test]
  fn copilot_cli_resolves_with_copilot_overlay() {
    let r = lookup("copilot", &Persona::CopilotCli).unwrap();
    assert_eq!(r.persona, PersonaKind::CopilotCli);
    assert_eq!(r.overlay, Some(OverlayKind::Copilot));
  }

  #[test]
  fn compose_overlay_wins_on_conflict() {
    let mut persona_map = HeaderMap::new();
    persona_map.insert(
      HeaderName::new("X-Session-Id"),
      HeaderValue::from_string("from-persona".into()),
    );
    persona_map.insert("X-Persona-Only", HeaderValue::from_string("p".into()));
    let mut overlay_map = HeaderMap::new();
    overlay_map.insert(
      HeaderName::new("x-session-id"),
      HeaderValue::from_string("from-overlay".into()),
    );
    overlay_map.insert("X-Overlay-Only", HeaderValue::from_string("o".into()));

    let composed = ResolvedSchema::compose(persona_map, Some(overlay_map));
    assert_eq!(composed.get(&keys::X_SESSION_ID).unwrap().as_str(), "from-overlay");
    assert_eq!(composed.get("X-Persona-Only").unwrap().as_str(), "p");
    assert_eq!(composed.get("X-Overlay-Only").unwrap().as_str(), "o");
  }

  #[test]
  fn compose_without_overlay_is_identity() {
    let mut persona_map = HeaderMap::new();
    persona_map.insert("A", HeaderValue::from_string("1".into()));
    let composed = ResolvedSchema::compose(persona_map.clone(), None);
    assert_eq!(composed, persona_map);
  }
}

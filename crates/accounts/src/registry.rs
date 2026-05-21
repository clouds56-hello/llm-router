use std::collections::BTreeMap;
use std::sync::Arc;
use tokn_auth::descriptor::{ProviderDescriptor, RewriteTarget};
use tokn_core::account::AccountConfig;
use tokn_core::provider::{error, Endpoint, Provider, Result};

pub struct Registry {
  descriptors: BTreeMap<&'static str, &'static ProviderDescriptor>,
}

impl Registry {
  pub fn builtin() -> Self {
    let mut r = Self {
      descriptors: BTreeMap::new(),
    };
    for d in builtin_descriptors() {
      r.register(d);
    }
    r
  }

  pub fn register(&mut self, descriptor: &'static ProviderDescriptor) {
    self.descriptors.insert(descriptor.id, descriptor);
  }

  pub fn resolve(&self, id: &str) -> Option<&'static ProviderDescriptor> {
    self.descriptors.get(id).copied()
  }

  pub fn iter(&self) -> impl Iterator<Item = &'static ProviderDescriptor> + '_ {
    self.descriptors.values().copied()
  }

  /// All known provider ids in registration order.
  pub fn ids(&self) -> Vec<&'static str> {
    self.descriptors.values().map(|d| d.id).collect()
  }

  /// Union of every descriptor's `hosts` field.
  pub fn intercept_hosts(&self) -> impl Iterator<Item = &'static str> + '_ {
    self.descriptors.values().flat_map(|d| d.hosts.iter().copied())
  }

  pub fn endpoint_path(&self, endpoint: Endpoint) -> Option<&'static str> {
    self
      .descriptors
      .values()
      .find_map(|descriptor| descriptor.endpoint_path(endpoint))
  }

  pub fn validate(&self, account: &AccountConfig) -> Result<()> {
    let descriptor = self
      .resolve(&account.provider)
      .ok_or_else(|| error::Error::UnknownProvider {
        id: account.provider.clone(),
        account: account.id.clone(),
      })?;
    (descriptor.validate)(account)
  }

  pub fn build(&self, account: Arc<AccountConfig>) -> Result<Arc<dyn Provider>> {
    self.validate(&account)?;
    let descriptor = self.resolve(&account.provider).expect("validated provider descriptor");
    (descriptor.build)(account)
  }

  pub fn provider_id_for_url(&self, url_or_host: &str) -> Option<&'static str> {
    let target = normalize_target(url_or_host)?;
    let host_matches = self
      .descriptors
      .values()
      .copied()
      .filter(|descriptor| descriptor.matches_host(&target.host))
      .collect::<Vec<_>>();
    // Prefer a descriptor whose `matches_url` claims the (host, path).
    if let Some(descriptor) = host_matches
      .iter()
      .copied()
      .find(|descriptor| descriptor.matches_url(&target.host, &target.path))
    {
      return Some(descriptor.id);
    }
    // Fall back to the lone host claimant only when the path is empty
    // (i.e. caller passed a bare host, not a URL); requires path
    // validation otherwise so that unrelated paths on a shared host
    // don't accidentally resolve.
    match host_matches.as_slice() {
      [descriptor] if target.path.is_empty() || target.path == "/" => Some(descriptor.id),
      _ => None,
    }
  }

  /// Best-effort route rewrite for an inbound `(host, method, path)`.
  /// Returns a typed target when a registered provider claims the host
  /// and recognises the path; falls back to `None` otherwise.
  ///
  /// Universal `GET /v1/models` is handled by the proxy as a global rule
  /// — kept outside the descriptor table because it applies regardless
  /// of host.
  pub fn rewrite_target(&self, host: &str, method: &str, path: &str) -> Option<RewriteTarget> {
    let host = host.to_ascii_lowercase();
    self
      .descriptors
      .values()
      .filter(|d| d.matches_host(&host))
      .find_map(|d| d.rewrite(method, path))
  }
}

fn builtin_descriptors() -> &'static [&'static ProviderDescriptor] {
  static LIST: &[&ProviderDescriptor] = &[
    &tokn_provider_copilot::DESCRIPTOR,
    &tokn_provider_deepseek::DESCRIPTOR,
    &tokn_provider_llama_cpp::DESCRIPTOR,
    &tokn_provider_openai::DESCRIPTOR_OPENAI,
    &tokn_provider_openai::DESCRIPTOR_CODEX,
    &tokn_provider_zai::DESCRIPTOR_ZAI,
    &tokn_provider_zai::DESCRIPTOR_ZAI_CODING_PLAN,
    &tokn_provider_zai::DESCRIPTOR_ZHIPUAI,
    &tokn_provider_zai::DESCRIPTOR_ZHIPUAI_CODING_PLAN,
  ];
  LIST
}

struct NormalizedTarget {
  host: String,
  path: String,
}

fn normalize_target(raw: &str) -> Option<NormalizedTarget> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  if let Ok(url) = reqwest::Url::parse(raw) {
    if let Some(host) = url.host_str() {
      let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
      return Some(NormalizedTarget {
        host,
        path: url.path().to_string(),
      });
    }
  }
  let without_scheme = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
  let authority = without_scheme
    .split(['/', '?', '#'])
    .next()
    .unwrap_or(without_scheme)
    .trim();
  let authority = authority.trim();
  if authority.is_empty() {
    return None;
  }
  let path = without_scheme
    .strip_prefix(authority)
    .and_then(|rest| rest.strip_prefix('/'))
    .map(|rest| {
      let path = rest.split(['?', '#']).next().unwrap_or(rest);
      format!("/{path}")
    })
    .unwrap_or_default();
  let without_userinfo = authority.rsplit('@').next().unwrap_or(authority);
  let host = if without_userinfo.starts_with('[') {
    without_userinfo
      .split_once(']')
      .map(|(host, _)| format!("{host}]"))
      .unwrap_or_else(|| without_userinfo.to_string())
  } else {
    without_userinfo
      .split_once(':')
      .map(|(host, _)| host.to_string())
      .unwrap_or_else(|| without_userinfo.to_string())
  };
  let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
  (!host.is_empty()).then_some(NormalizedTarget { host, path })
}

pub fn build_for_account(account: Arc<AccountConfig>) -> Result<Arc<dyn Provider>> {
  Registry::builtin().build(account)
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokn_auth::descriptor::RewriteTarget;
  use tokn_core::provider::{
    Endpoint, ID_CODEX, ID_DEEPSEEK, ID_GITHUB_COPILOT, ID_LLAMA_CPP, ID_OPENAI, ID_ZAI, ID_ZAI_CODING_PLAN,
    ID_ZHIPUAI, ID_ZHIPUAI_CODING_PLAN,
  };

  #[test]
  fn registry_matches_provider_hosts() {
    let registry = Registry::builtin();
    assert_eq!(registry.provider_id_for_url("api.github.com"), Some(ID_GITHUB_COPILOT));
    assert_eq!(
      registry.provider_id_for_url("api.githubcopilot.com"),
      Some(ID_GITHUB_COPILOT)
    );
    assert_eq!(registry.provider_id_for_url("api.z.ai"), Some(ID_ZAI));
    assert_eq!(registry.provider_id_for_url("open.bigmodel.cn"), Some(ID_ZHIPUAI));
    assert_eq!(registry.provider_id_for_url("api.deepseek.com"), Some(ID_DEEPSEEK));
    assert_eq!(registry.provider_id_for_url("localhost"), Some(ID_LLAMA_CPP));
    assert_eq!(registry.provider_id_for_url("127.0.0.1"), Some(ID_LLAMA_CPP));
    assert_eq!(registry.provider_id_for_url("api.openai.com"), Some(ID_OPENAI));
    assert_eq!(
      registry.provider_id_for_url("chatgpt.com/backend-api/codex/responses"),
      Some(ID_CODEX)
    );
  }

  #[test]
  fn registry_distinguishes_zai_url_prefixes() {
    let registry = Registry::builtin();
    assert_eq!(
      registry.provider_id_for_url("https://api.z.ai/api/coding/paas/v4/chat/completions"),
      Some(ID_ZAI_CODING_PLAN)
    );
    assert_eq!(
      registry.provider_id_for_url("https://api.z.ai/api/paas/v4/chat/completions"),
      Some(ID_ZAI)
    );
    assert_eq!(
      registry.provider_id_for_url("https://open.bigmodel.cn/api/coding/paas/v4/chat/completions"),
      Some(ID_ZHIPUAI_CODING_PLAN)
    );
    assert_eq!(
      registry.provider_id_for_url("https://open.bigmodel.cn/api/paas/v4/chat/completions"),
      Some(ID_ZHIPUAI)
    );
  }

  #[test]
  fn registry_normalizes_url_inputs() {
    let registry = Registry::builtin();
    assert_eq!(
      registry.provider_id_for_url("HTTPS://API.GITHUBCOPILOT.COM:443/v1/chat/completions"),
      Some(ID_GITHUB_COPILOT)
    );
    assert_eq!(registry.provider_id_for_url("open.bigmodel.cn:443"), Some(ID_ZHIPUAI));
  }

  #[test]
  fn registry_does_not_invent_unknown_provider_ids() {
    let registry = Registry::builtin();
    assert_eq!(registry.provider_id_for_url("api.anthropic.com"), None);
    assert_eq!(registry.provider_id_for_url("openrouter.ai"), None);
    assert_eq!(registry.provider_id_for_url("chatgpt.com/backend-api/unknown"), None);
  }

  #[test]
  fn registry_rewrites_canonical_and_aliased_paths() {
    let registry = Registry::builtin();
    // Canonical path → no-op rewrite (still recognised).
    assert_eq!(
      registry.rewrite_target("api.openai.com", "POST", "/v1/chat/completions"),
      Some(RewriteTarget::Endpoint(Endpoint::ChatCompletions))
    );
    // Aliased deepseek path → canonical.
    assert_eq!(
      registry.rewrite_target("api.deepseek.com", "POST", "/chat/completions"),
      Some(RewriteTarget::Endpoint(Endpoint::ChatCompletions))
    );
    assert_eq!(
      registry.rewrite_target("api.deepseek.com", "POST", "/anthropic/v1/messages"),
      Some(RewriteTarget::Endpoint(Endpoint::Messages))
    );
    // Codex non-canonical inbound path.
    assert_eq!(
      registry.rewrite_target("chatgpt.com", "POST", "/backend-api/codex/responses"),
      Some(RewriteTarget::Endpoint(Endpoint::Responses))
    );
    // Unknown host → None.
    assert_eq!(
      registry.rewrite_target("api.anthropic.com", "POST", "/v1/messages"),
      None
    );
  }

  #[test]
  fn registry_intercept_hosts_covers_known_providers() {
    let registry = Registry::builtin();
    let hosts: std::collections::HashSet<&'static str> = registry.intercept_hosts().collect();
    for host in [
      "api.github.com",
      "api.githubcopilot.com",
      "api.openai.com",
      "chatgpt.com",
      "api.deepseek.com",
      "api.z.ai",
      "open.bigmodel.cn",
    ] {
      assert!(hosts.contains(host), "missing default intercept host {host}");
    }
  }

  #[test]
  fn descriptors_are_well_formed() {
    let registry = Registry::builtin();
    for d in registry.iter() {
      assert!(!d.id.is_empty(), "descriptor with empty id");
      assert!(!d.display_name.is_empty(), "{} has empty display_name", d.id);
      assert!(!d.base_url.is_empty(), "{} has empty base_url", d.id);
      assert!(!d.hosts.is_empty(), "{} has no hosts", d.id);
      assert!(!d.endpoints.is_empty(), "{} has no endpoints", d.id);
      assert!(!d.credentials.is_empty(), "{} has no credentials", d.id);
      assert!(d.build_auth.is_some(), "{} has no build_auth", d.id);
    }
  }

  #[test]
  fn codex_descriptor_exposes_required_auth_urls() {
    let registry = Registry::builtin();
    let codex = registry.resolve(ID_CODEX).expect("codex descriptor");
    for name in [
      "device_usercode",
      "device_token",
      "oauth_token",
      "device_verify",
      "device_redirect",
    ] {
      assert!(
        codex.auth_url(name).is_some(),
        "codex descriptor missing auth_url('{name}')"
      );
    }
  }
}

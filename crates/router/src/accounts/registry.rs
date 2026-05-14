use llm_core::account::AccountConfig;
use llm_core::provider::{error, Provider, ProviderDescriptor, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct Registry {
  descriptors: BTreeMap<&'static str, &'static ProviderDescriptor>,
}

impl Registry {
  pub fn builtin() -> Self {
    let mut r = Self {
      descriptors: BTreeMap::new(),
    };
    r.register(&llm_provider_copilot::DESCRIPTOR);
    r.register(&llm_provider_deepseek::DESCRIPTOR);
    r.register(&llm_provider_openai::DESCRIPTOR_OPENAI);
    r.register(&llm_provider_openai::DESCRIPTOR_CODEX);
    r.register(&llm_provider_zai::DESCRIPTOR_ZAI);
    r.register(&llm_provider_zai::DESCRIPTOR_ZAI_CODING_PLAN);
    r.register(&llm_provider_zai::DESCRIPTOR_ZHIPUAI);
    r.register(&llm_provider_zai::DESCRIPTOR_ZHIPUAI_CODING_PLAN);
    r
  }

  pub fn register(&mut self, descriptor: &'static ProviderDescriptor) {
    self.descriptors.insert(descriptor.id, descriptor);
  }

  pub fn resolve(&self, id: &str) -> Option<&'static ProviderDescriptor> {
    self.descriptors.get(id).copied()
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
    let matches = self
      .descriptors
      .values()
      .copied()
      .filter(|descriptor| descriptor.matches_host(&target.host))
      .collect::<Vec<_>>();
    match matches.as_slice() {
      [] => None,
      [descriptor] => Some(descriptor.id),
      _ => matches
        .into_iter()
        .find(|descriptor| descriptor.matches_url(&target.host, &target.path))
        .map(|descriptor| descriptor.id),
    }
  }
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
  use llm_core::provider::{
    ID_CODEX, ID_DEEPSEEK, ID_GITHUB_COPILOT, ID_OPENAI, ID_ZAI, ID_ZAI_CODING_PLAN, ID_ZHIPUAI, ID_ZHIPUAI_CODING_PLAN,
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
}

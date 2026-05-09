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
}

pub fn build_for_account(account: Arc<AccountConfig>) -> Result<Arc<dyn Provider>> {
  Registry::builtin().build(account)
}

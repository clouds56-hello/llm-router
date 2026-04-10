use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::{Config as NotifyConfig, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::logging::InMemoryLogSink;

const ENC2_PREFIX: &str = "enc2:";
const ENC2_VERSION: &str = "v1";
const ENC2_ALGO: &str = "xor";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersFile {
  pub providers: HashMap<String, ProviderDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDefinition {
  pub provider_type: String,
  pub base_url: String,
  #[serde(default = "default_true")]
  pub enabled: bool,
  #[serde(default)]
  pub metadata: HashMap<String, String>,
}

fn default_true() -> bool {
  true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelsFile {
  #[serde(default)]
  pub models: Vec<ModelRoute>,
  pub default_provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRoute {
  pub openai_name: String,
  pub provider: String,
  pub provider_model: String,
  #[serde(default)]
  pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialsFile {
  #[serde(default)]
  pub providers: HashMap<String, ProviderCredentialConfig>,
  pub copilot: Option<CopilotCredentialSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderCredentialConfig {
  #[serde(default)]
  pub accounts: Vec<CredentialAccount>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub api_key: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub auth_type: Option<String>,
  #[serde(default, skip_serializing_if = "HashMap::is_empty")]
  pub extra: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialAccount {
  pub id: String,
  pub label: String,
  #[serde(default)]
  pub auth_type: Option<String>,
  #[serde(default)]
  pub is_default: bool,
  #[serde(default = "default_true")]
  pub enabled: bool,
  #[serde(default)]
  pub secrets: HashMap<String, String>,
  #[serde(default)]
  pub meta: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderCredential {
  pub api_key: Option<String>,
  #[serde(default)]
  pub auth_type: Option<String>,
  #[serde(default)]
  pub extra: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CopilotCredentialSettings {
  pub deployment: Option<String>,
  pub enterprise_domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedConfig {
  pub providers: ProvidersFile,
  pub models: ModelsFile,
  pub credentials: CredentialsFile,
}

impl LoadedConfig {
  pub fn resolve_model(&self, model: &str) -> Option<&ModelRoute> {
    self.models.models.iter().find(|m| m.openai_name == model).or_else(|| {
      self
        .models
        .models
        .iter()
        .find(|m| m.is_default)
        .or_else(|| self.models.models.first())
    })
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountView {
  pub provider: String,
  pub id: String,
  pub label: String,
  pub auth_type: Option<String>,
  pub is_default: bool,
  pub enabled: bool,
  pub meta: HashMap<String, String>,
  pub secret_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConnectAccountInput {
  pub provider: String,
  pub account_id: Option<String>,
  pub label: Option<String>,
  pub auth_type: Option<String>,
  #[serde(default)]
  pub secrets: HashMap<String, String>,
  #[serde(default)]
  pub meta: HashMap<String, String>,
  pub set_default: Option<bool>,
  pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateAccountInput {
  pub provider: String,
  pub account_id: String,
  pub label: Option<String>,
  pub auth_type: Option<String>,
  pub enabled: Option<bool>,
  pub set_default: Option<bool>,
  pub clear_secret_keys: Option<Vec<String>>,
  pub set_secrets: Option<HashMap<String, String>>,
  pub set_meta: Option<HashMap<String, String>>,
  pub clear_meta_keys: Option<Vec<String>>,
}

impl CredentialsFile {
  fn normalize_v2(&mut self) {
    for (provider_name, provider_cfg) in &mut self.providers {
      if provider_cfg.accounts.is_empty() && provider_cfg.api_key.is_some() {
        let mut secrets = provider_cfg.extra.clone();
        if let Some(api_key) = provider_cfg.api_key.take() {
          secrets.insert("api_key".to_string(), api_key);
        }
        let account = CredentialAccount {
          id: format!("{}-default", sanitize_id(provider_name)),
          label: "Default".to_string(),
          auth_type: provider_cfg.auth_type.clone(),
          is_default: true,
          enabled: true,
          secrets,
          meta: HashMap::new(),
        };
        provider_cfg.accounts.push(account);
      }

      provider_cfg.api_key = None;
      provider_cfg.auth_type = None;
      provider_cfg.extra.clear();

      if provider_cfg.accounts.is_empty() {
        continue;
      }

      let mut default_seen = false;
      for account in &mut provider_cfg.accounts {
        if account.id.trim().is_empty() {
          account.id = format!("{}-{}", sanitize_id(provider_name), uuid::Uuid::new_v4());
        }
        if account.label.trim().is_empty() {
          account.label = account.id.clone();
        }
        if account.is_default {
          if default_seen {
            account.is_default = false;
          } else {
            default_seen = true;
          }
        }
      }

      if !default_seen {
        if let Some(account) = provider_cfg.accounts.iter_mut().find(|a| a.enabled) {
          account.is_default = true;
        } else if let Some(first) = provider_cfg.accounts.first_mut() {
          first.is_default = true;
        }
      }
    }
  }

  fn validate_enc2_values(&self) -> Result<()> {
    for (provider, provider_cfg) in &self.providers {
      for account in &provider_cfg.accounts {
        for (key, value) in &account.secrets {
          if value.starts_with(ENC2_PREFIX) {
            decode_inline_secret(value).with_context(|| {
              format!(
                "invalid enc2 value for provider='{provider}' account='{}' secret='{key}'",
                account.id
              )
            })?;
          }
        }
      }
    }
    Ok(())
  }

  pub fn list_accounts(&self) -> Vec<AccountView> {
    let mut out = Vec::new();
    for (provider, provider_cfg) in &self.providers {
      for account in &provider_cfg.accounts {
        let mut secret_keys = account.secrets.keys().cloned().collect::<Vec<_>>();
        secret_keys.sort();
        out.push(AccountView {
          provider: provider.clone(),
          id: account.id.clone(),
          label: account.label.clone(),
          auth_type: account.auth_type.clone(),
          is_default: account.is_default,
          enabled: account.enabled,
          meta: account.meta.clone(),
          secret_keys,
        });
      }
    }
    out
  }

  pub fn resolve_runtime_credential(&self, provider: &str) -> Result<Option<ProviderCredential>> {
    let Some(provider_cfg) = self.providers.get(provider) else {
      return Ok(None);
    };

    let account = provider_cfg
      .accounts
      .iter()
      .find(|a| a.is_default && a.enabled)
      .or_else(|| provider_cfg.accounts.iter().find(|a| a.enabled))
      .or_else(|| provider_cfg.accounts.first());

    let Some(account) = account else {
      return Ok(None);
    };

    let mut extra = HashMap::new();
    for (key, raw) in &account.secrets {
      let decoded = decode_inline_secret(raw).with_context(|| {
        format!(
          "failed to decode secret '{key}' for provider='{provider}' account='{}'",
          account.id
        )
      })?;
      if key == "api_key" {
        continue;
      }
      extra.insert(key.clone(), decoded);
    }

    let api_key = match account.secrets.get("api_key") {
      Some(v) => Some(decode_inline_secret(v).with_context(|| {
        format!(
          "failed to decode api_key for provider='{provider}' account='{}'",
          account.id
        )
      })?),
      None => None,
    };

    Ok(Some(ProviderCredential {
      api_key,
      auth_type: account.auth_type.clone(),
      extra,
    }))
  }
}

#[derive(Clone)]
pub struct ConfigManager {
  config_dir: PathBuf,
  current: Arc<RwLock<LoadedConfig>>,
  last_error: Arc<RwLock<Option<String>>>,
  watcher: Arc<RwLock<Option<RecommendedWatcher>>>,
  logger: InMemoryLogSink,
}

impl ConfigManager {
  pub fn new(config_dir: PathBuf, logger: InMemoryLogSink) -> Result<Self> {
    let loaded = Self::load_from_dir(&config_dir)?;
    Ok(Self {
      config_dir,
      current: Arc::new(RwLock::new(loaded)),
      last_error: Arc::new(RwLock::new(None)),
      watcher: Arc::new(RwLock::new(None)),
      logger,
    })
  }

  pub fn get(&self) -> LoadedConfig {
    self.current.read().clone()
  }

  pub fn last_error(&self) -> Option<String> {
    self.last_error.read().clone()
  }

  pub fn reload(&self) -> Result<()> {
    match Self::load_from_dir(&self.config_dir) {
      Ok(cfg) => {
        *self.current.write() = cfg;
        *self.last_error.write() = None;
        self.logger.info("config", "reloaded YAML config");
        Ok(())
      }
      Err(err) => {
        let msg = err.to_string();
        *self.last_error.write() = Some(msg.clone());
        self.logger.error("config", format!("reload failed: {msg}"));
        Err(err)
      }
    }
  }

  pub fn start_hot_reload(&self) -> Result<()> {
    let this = self.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
      if let Ok(event) = res {
        match event.kind {
          EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
            let _ = this.reload();
          }
          _ => {}
        }
      }
    })
    .context("failed to create config watcher")?;

    watcher
      .configure(NotifyConfig::default())
      .context("failed to configure watcher")?;

    watcher
      .watch(&self.config_dir, RecursiveMode::NonRecursive)
      .context("failed to watch config directory")?;

    *self.watcher.write() = Some(watcher);
    Ok(())
  }

  pub fn list_accounts(&self) -> Vec<AccountView> {
    self.current.read().credentials.list_accounts()
  }

  pub fn connect_account(&self, input: ConnectAccountInput) -> Result<AccountView> {
    if input.provider.trim().is_empty() {
      anyhow::bail!("provider is required");
    }

    let mut loaded = self.current.read().clone();
    let provider = input.provider.clone();
    let provider_cfg = loaded.credentials.providers.entry(provider.clone()).or_default();

    let account_id = input
      .account_id
      .filter(|v| !v.trim().is_empty())
      .unwrap_or_else(|| format!("{}-{}", sanitize_id(&provider), uuid::Uuid::new_v4()));

    let mut secrets = HashMap::new();
    for (k, v) in input.secrets {
      secrets.insert(k, encode_inline_secret(&v));
    }

    let mut updated_existing = false;
    for account in &mut provider_cfg.accounts {
      if account.id == account_id {
        updated_existing = true;
        if let Some(label) = &input.label {
          account.label = label.clone();
        }
        if let Some(auth_type) = &input.auth_type {
          account.auth_type = Some(auth_type.clone());
        }
        if let Some(enabled) = input.enabled {
          account.enabled = enabled;
        }
        for (k, v) in &secrets {
          account.secrets.insert(k.clone(), v.clone());
        }
        for (k, v) in &input.meta {
          account.meta.insert(k.clone(), v.clone());
        }
      }
    }

    if !updated_existing {
      let account = CredentialAccount {
        id: account_id.clone(),
        label: input.label.unwrap_or_else(|| account_id.clone()),
        auth_type: input.auth_type,
        is_default: false,
        enabled: input.enabled.unwrap_or(true),
        secrets,
        meta: input.meta,
      };
      provider_cfg.accounts.push(account);
    }

    if input.set_default.unwrap_or(false) {
      for account in &mut provider_cfg.accounts {
        account.is_default = account.id == account_id;
      }
    }

    loaded.credentials.normalize_v2();
    self.persist_and_swap(loaded)?;

    self
      .list_accounts()
      .into_iter()
      .find(|a| a.provider == provider && a.id == account_id)
      .ok_or_else(|| anyhow::anyhow!("failed to read back connected account"))
  }

  pub fn disconnect_account(&self, provider: &str, account_id: &str) -> Result<()> {
    let mut loaded = self.current.read().clone();
    let Some(provider_cfg) = loaded.credentials.providers.get_mut(provider) else {
      return Ok(());
    };

    provider_cfg.accounts.retain(|a| a.id != account_id);
    loaded.credentials.normalize_v2();
    self.persist_and_swap(loaded)
  }

  pub fn set_default_account(&self, provider: &str, account_id: &str) -> Result<()> {
    let mut loaded = self.current.read().clone();
    let Some(provider_cfg) = loaded.credentials.providers.get_mut(provider) else {
      anyhow::bail!("provider '{}' not found", provider);
    };

    let mut found = false;
    for account in &mut provider_cfg.accounts {
      account.is_default = account.id == account_id;
      if account.is_default {
        account.enabled = true;
        found = true;
      }
    }

    if !found {
      anyhow::bail!("account '{}' not found for provider '{}'", account_id, provider);
    }

    loaded.credentials.normalize_v2();
    self.persist_and_swap(loaded)
  }

  pub fn update_account(&self, input: UpdateAccountInput) -> Result<AccountView> {
    let mut loaded = self.current.read().clone();
    let provider = input.provider.clone();

    let provider_cfg = loaded
      .credentials
      .providers
      .get_mut(&provider)
      .ok_or_else(|| anyhow::anyhow!("provider '{}' not found", provider))?;

    let account = provider_cfg
      .accounts
      .iter_mut()
      .find(|a| a.id == input.account_id)
      .ok_or_else(|| anyhow::anyhow!("account '{}' not found for provider '{}'", input.account_id, provider))?;

    if let Some(label) = input.label {
      account.label = label;
    }
    if let Some(auth_type) = input.auth_type {
      account.auth_type = Some(auth_type);
    }
    if let Some(enabled) = input.enabled {
      account.enabled = enabled;
    }

    if let Some(keys) = input.clear_secret_keys {
      for key in keys {
        account.secrets.remove(&key);
      }
    }

    if let Some(secrets) = input.set_secrets {
      for (k, v) in secrets {
        account.secrets.insert(k, encode_inline_secret(&v));
      }
    }

    if let Some(meta) = input.set_meta {
      for (k, v) in meta {
        account.meta.insert(k, v);
      }
    }

    if let Some(keys) = input.clear_meta_keys {
      for key in keys {
        account.meta.remove(&key);
      }
    }

    if input.set_default.unwrap_or(false) {
      for current in &mut provider_cfg.accounts {
        current.is_default = current.id == input.account_id;
      }
    }

    loaded.credentials.normalize_v2();
    self.persist_and_swap(loaded)?;

    self
      .list_accounts()
      .into_iter()
      .find(|a| a.provider == provider && a.id == input.account_id)
      .ok_or_else(|| anyhow::anyhow!("failed to read back updated account"))
  }

  pub fn load_from_dir(config_dir: &Path) -> Result<LoadedConfig> {
    let providers: ProvidersFile = read_yaml(config_dir.join("providers.yaml"))?;
    let models: ModelsFile = read_yaml(config_dir.join("models.yaml"))?;
    let mut credentials: CredentialsFile = read_yaml(config_dir.join("credentials.yaml"))?;

    credentials.normalize_v2();
    credentials.validate_enc2_values()?;

    if models.models.is_empty() {
      anyhow::bail!("models.yaml must include at least one model route");
    }

    for model in &models.models {
      if !providers.providers.contains_key(&model.provider) {
        anyhow::bail!(
          "model '{}' references unknown provider '{}'",
          model.openai_name,
          model.provider
        );
      }
    }

    Ok(LoadedConfig {
      providers,
      models,
      credentials,
    })
  }

  fn persist_and_swap(&self, mut loaded: LoadedConfig) -> Result<()> {
    loaded.credentials.normalize_v2();
    loaded.credentials.validate_enc2_values()?;

    let path = self.config_dir.join("credentials.yaml");
    write_yaml_atomic(path, &loaded.credentials)?;

    *self.current.write() = loaded;
    *self.last_error.write() = None;
    self.logger.info("config", "updated credentials.yaml");
    Ok(())
  }
}

fn write_yaml_atomic<T: Serialize>(path: PathBuf, value: &T) -> Result<()> {
  let bytes = serde_yaml::to_string(value).context("failed to serialize yaml")?;
  let tmp = path.with_extension("yaml.tmp");
  std::fs::write(&tmp, bytes).with_context(|| format!("failed writing {}", tmp.display()))?;
  std::fs::rename(&tmp, &path).with_context(|| format!("failed replacing {}", path.display()))?;
  Ok(())
}

fn read_yaml<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T> {
  let content = std::fs::read_to_string(&path).with_context(|| format!("failed reading {}", path.display()))?;
  serde_yaml::from_str(&content).with_context(|| format!("invalid YAML in {}", path.display()))
}

pub fn encode_inline_secret(plain: &str) -> String {
  let nonce = *uuid::Uuid::new_v4().as_bytes();
  let keystream = derive_keystream(&nonce);
  let mut out = Vec::with_capacity(plain.len());
  for (idx, b) in plain.as_bytes().iter().enumerate() {
    out.push(*b ^ keystream[idx % keystream.len()] ^ nonce[idx % nonce.len()]);
  }

  format!(
    "{ENC2_PREFIX}{ENC2_VERSION}.{ENC2_ALGO}.{}.{}",
    hex_encode(&nonce),
    hex_encode(&out)
  )
}

pub fn decode_inline_secret(raw: &str) -> Result<String> {
  if !raw.starts_with(ENC2_PREFIX) {
    return Ok(raw.to_string());
  }

  let payload = raw
    .strip_prefix(ENC2_PREFIX)
    .ok_or_else(|| anyhow::anyhow!("missing enc2 prefix"))?;
  let parts: Vec<&str> = payload.split('.').collect();
  if parts.len() != 4 {
    anyhow::bail!("enc2 payload must contain 4 dot-separated parts");
  }
  if parts[0] != ENC2_VERSION {
    anyhow::bail!("unsupported enc2 version '{}'", parts[0]);
  }
  if parts[1] != ENC2_ALGO {
    anyhow::bail!("unsupported enc2 algorithm '{}'", parts[1]);
  }

  let nonce = hex_decode(parts[2]).context("invalid nonce hex")?;
  if nonce.len() != 16 {
    anyhow::bail!("invalid nonce size {}", nonce.len());
  }

  let cipher = hex_decode(parts[3]).context("invalid ciphertext hex")?;
  let keystream = derive_keystream(
    nonce
      .as_slice()
      .try_into()
      .map_err(|_| anyhow::anyhow!("nonce conversion failed"))?,
  );

  let mut out = Vec::with_capacity(cipher.len());
  for (idx, b) in cipher.iter().enumerate() {
    out.push(*b ^ keystream[idx % keystream.len()] ^ nonce[idx % nonce.len()]);
  }

  String::from_utf8(out).context("decoded enc2 bytes are not utf8")
}

fn derive_keystream(nonce: &[u8; 16]) -> [u8; 32] {
  let mut out = [0u8; 32];
  for idx in 0..32 {
    let n = nonce[idx % nonce.len()];
    let mixed = n
      .wrapping_mul(31)
      .wrapping_add((idx as u8).wrapping_mul(17))
      .rotate_left((idx % 8) as u32)
      ^ 0xA7;
    out[idx] = mixed;
  }
  out
}

fn hex_encode(bytes: &[u8]) -> String {
  const HEX: &[u8; 16] = b"0123456789abcdef";
  let mut out = String::with_capacity(bytes.len() * 2);
  for b in bytes {
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0x0f) as usize] as char);
  }
  out
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
  if !value.len().is_multiple_of(2) {
    anyhow::bail!("hex length must be even");
  }

  let mut out = Vec::with_capacity(value.len() / 2);
  let bytes = value.as_bytes();
  let mut idx = 0;
  while idx < bytes.len() {
    let hi = from_hex_char(bytes[idx] as char).ok_or_else(|| anyhow::anyhow!("invalid hex char"))?;
    let lo = from_hex_char(bytes[idx + 1] as char).ok_or_else(|| anyhow::anyhow!("invalid hex char"))?;
    out.push((hi << 4) | lo);
    idx += 2;
  }
  Ok(out)
}

fn from_hex_char(c: char) -> Option<u8> {
  match c {
    '0'..='9' => Some(c as u8 - b'0'),
    'a'..='f' => Some(c as u8 - b'a' + 10),
    'A'..='F' => Some(c as u8 - b'A' + 10),
    _ => None,
  }
}

fn sanitize_id(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  for ch in input.chars() {
    if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
      out.push(ch.to_ascii_lowercase());
    } else {
      out.push('-');
    }
  }
  if out.is_empty() {
    "account".to_string()
  } else {
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn enc2_roundtrip_works() {
    let secret = "sk-test-123";
    let encoded = encode_inline_secret(secret);
    assert!(encoded.starts_with("enc2:"));
    let decoded = decode_inline_secret(&encoded).unwrap();
    assert_eq!(decoded, secret);
  }

  #[test]
  fn legacy_credentials_are_migrated() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    std::fs::write(
      tmp.path().join("providers.yaml"),
      "providers:\n  openai:\n    provider_type: openai\n    base_url: https://api.openai.com\n    enabled: true\n",
    )
    .unwrap();
    std::fs::write(
      tmp.path().join("models.yaml"),
      "models:\n  - openai_name: gpt-4.1-mini\n    provider: openai\n    provider_model: gpt-4.1-mini\n    is_default: true\n",
    )
    .unwrap();
    std::fs::write(
      tmp.path().join("credentials.yaml"),
      "providers:\n  openai:\n    api_key: legacy-key\n    auth_type: bearer\n",
    )
    .unwrap();

    let logger = InMemoryLogSink::new(100);
    let manager = ConfigManager::new(tmp.path().to_path_buf(), logger).unwrap();
    let loaded = manager.get();
    let account = &loaded.credentials.providers["openai"].accounts[0];
    assert_eq!(account.auth_type.as_deref(), Some("bearer"));

    let runtime = loaded
      .credentials
      .resolve_runtime_credential("openai")
      .unwrap()
      .unwrap();
    assert_eq!(runtime.api_key.as_deref(), Some("legacy-key"));
  }

  #[test]
  fn invalid_enc2_fails_load() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    std::fs::write(
      tmp.path().join("providers.yaml"),
      "providers:\n  openai:\n    provider_type: openai\n    base_url: https://api.openai.com\n    enabled: true\n",
    )
    .unwrap();
    std::fs::write(
      tmp.path().join("models.yaml"),
      "models:\n  - openai_name: gpt-4.1-mini\n    provider: openai\n    provider_model: gpt-4.1-mini\n    is_default: true\n",
    )
    .unwrap();
    std::fs::write(
      tmp.path().join("credentials.yaml"),
      "providers:\n  openai:\n    accounts:\n      - id: a1\n        label: main\n        is_default: true\n        enabled: true\n        secrets:\n          api_key: 'enc2:bad'\n",
    )
    .unwrap();

    let logger = InMemoryLogSink::new(100);
    let err = match ConfigManager::new(tmp.path().to_path_buf(), logger) {
      Ok(_) => panic!("expected invalid enc2 to fail"),
      Err(err) => err,
    };
    assert!(err.to_string().contains("invalid enc2 value"));
  }

  #[test]
  fn connect_disconnect_and_default_work() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    std::fs::write(
      tmp.path().join("providers.yaml"),
      "providers:\n  openai:\n    provider_type: openai\n    base_url: https://api.openai.com\n    enabled: true\n",
    )
    .unwrap();
    std::fs::write(
      tmp.path().join("models.yaml"),
      "models:\n  - openai_name: gpt-4.1-mini\n    provider: openai\n    provider_model: gpt-4.1-mini\n    is_default: true\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("credentials.yaml"), "providers: {}\n").unwrap();

    let logger = InMemoryLogSink::new(100);
    let manager = ConfigManager::new(tmp.path().to_path_buf(), logger).unwrap();

    manager
      .connect_account(ConnectAccountInput {
        provider: "openai".to_string(),
        account_id: Some("a1".to_string()),
        label: Some("A1".to_string()),
        auth_type: Some("bearer".to_string()),
        secrets: HashMap::from_iter(vec![("api_key".to_string(), "k1".to_string())]),
        meta: HashMap::new(),
        set_default: Some(true),
        enabled: Some(true),
      })
      .unwrap();

    manager
      .connect_account(ConnectAccountInput {
        provider: "openai".to_string(),
        account_id: Some("a2".to_string()),
        label: Some("A2".to_string()),
        auth_type: Some("bearer".to_string()),
        secrets: HashMap::from_iter(vec![("api_key".to_string(), "k2".to_string())]),
        meta: HashMap::new(),
        set_default: Some(false),
        enabled: Some(true),
      })
      .unwrap();

    manager.set_default_account("openai", "a2").unwrap();
    let runtime = manager
      .get()
      .credentials
      .resolve_runtime_credential("openai")
      .unwrap()
      .unwrap();
    assert_eq!(runtime.api_key.as_deref(), Some("k2"));

    manager.disconnect_account("openai", "a2").unwrap();
    let runtime = manager
      .get()
      .credentials
      .resolve_runtime_credential("openai")
      .unwrap()
      .unwrap();
    assert_eq!(runtime.api_key.as_deref(), Some("k1"));
  }
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::{Config as NotifyConfig, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::logging::InMemoryLogSink;

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
  pub providers: HashMap<String, ProviderCredential>,
  pub copilot: Option<CopilotCredentialSettings>,
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

  pub fn load_from_dir(config_dir: &Path) -> Result<LoadedConfig> {
    let providers: ProvidersFile = read_yaml(config_dir.join("providers.yaml"))?;
    let models: ModelsFile = read_yaml(config_dir.join("models.yaml"))?;
    let credentials: CredentialsFile = read_yaml(config_dir.join("credentials.yaml"))?;

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
}

fn read_yaml<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T> {
  let content = std::fs::read_to_string(&path).with_context(|| format!("failed reading {}", path.display()))?;
  serde_yaml::from_str(&content).with_context(|| format!("invalid YAML in {}", path.display()))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hot_reload_picks_up_changes() {
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
    let before = manager.get();
    assert_eq!(before.models.models[0].openai_name, "gpt-4.1-mini");

    std::fs::write(
      tmp.path().join("models.yaml"),
      "models:\n  - openai_name: gpt-4.1\n    provider: openai\n    provider_model: gpt-4.1\n    is_default: true\n",
    )
    .unwrap();

    manager.reload().unwrap();
    let after = manager.get();
    assert_eq!(after.models.models[0].openai_name, "gpt-4.1");
  }
}

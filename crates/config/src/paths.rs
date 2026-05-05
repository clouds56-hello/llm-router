use super::Result;
use std::path::PathBuf;

pub fn config_path() -> Result<PathBuf> {
  let dirs = super::project_dirs()?;
  Ok(dirs.config_dir().join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
  let dirs = super::project_dirs()?;
  Ok(dirs.data_dir().to_path_buf())
}

pub fn default_usage_db() -> Result<PathBuf> {
  Ok(data_dir()?.join("usage.db"))
}

pub fn default_sessions_db() -> Result<PathBuf> {
  Ok(data_dir()?.join("sessions.db"))
}

pub fn default_requests_dir() -> Result<PathBuf> {
  Ok(data_dir()?.join("requests"))
}

pub fn default_logs_dir() -> Result<PathBuf> {
  Ok(data_dir()?.join("logs"))
}

pub fn default_ca_dir() -> Result<PathBuf> {
  let dirs = super::project_dirs()?;
  Ok(dirs.config_dir().join("ca"))
}

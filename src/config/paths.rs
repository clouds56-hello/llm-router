use anyhow::Result;
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
    Ok(data_dir()?.join("usage.sqlite"))
}

use std::path::PathBuf;

pub const APP_QUALIFIER: &str = "dev";
pub const APP_ORGANIZATION: &str = "llm-router";
pub const APP_NAME: &str = "llm-router";

pub fn project_dirs() -> Option<directories::ProjectDirs> {
  directories::ProjectDirs::from(APP_QUALIFIER, APP_ORGANIZATION, APP_NAME)
}

pub fn config_dir() -> Option<PathBuf> {
  project_dirs().map(|dirs| dirs.config_dir().to_path_buf())
}

pub fn data_dir() -> Option<PathBuf> {
  project_dirs().map(|dirs| dirs.data_dir().to_path_buf())
}

pub fn cache_dir() -> Option<PathBuf> {
  project_dirs().map(|dirs| dirs.cache_dir().to_path_buf())
}

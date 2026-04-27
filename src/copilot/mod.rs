pub mod headers;
pub mod models;
pub mod oauth;
pub mod token;

#[allow(dead_code)]
pub const GITHUB_API: &str = "https://api.github.com";
pub const COPILOT_API: &str = "https://api.githubcopilot.com";
pub const TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";

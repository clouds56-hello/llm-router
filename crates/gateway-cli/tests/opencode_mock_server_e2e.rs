use tokn_mock_server::{MockAuthConfig, MockLlmConfig, MockLlmServer, MockRoute};
use serde_json::Value;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

#[tokio::test]
async fn curl_script_reaches_mock_server_with_opencode_headers() {
  let mock = MockLlmServer::start(
    MockLlmConfig {
      routes: vec![MockRoute::models(["gpt-4o-mini"]), MockRoute::chat_completions()],
      ..Default::default()
    }
    .with_auth(MockAuthConfig::bearer(["sk-test"])),
  )
  .await;

  let port = unused_port();
  let temp_root = temp_path("opencode-e2e");
  let config_dir = temp_root.join("xdg").join("tokn-router");
  fs::create_dir_all(&config_dir).unwrap();

  let config_path = config_dir.join("config.toml");
  let auth_path = config_dir.join("auth.yaml");
  write_config(&config_path, port);
  write_auth(&auth_path, mock.base_url());

  let mut gateway = spawn_gateway(&config_path, temp_root.join("xdg"));
  let gateway_base_url = format!("http://127.0.0.1:{port}");
  wait_for_gateway(&format!("{gateway_base_url}/v1/models")).await;

  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .and_then(Path::parent)
    .expect("repo root")
    .to_path_buf();
  let script = repo_root.join("scripts/opencode-curl-chat.sh");
  let output = Command::new("sh")
    .arg(&script)
    .arg(&gateway_base_url)
    .output()
    .await
    .expect("run curl script");

  if !output.status.success() {
    let logs = gateway_logs(&mut gateway).await;
    panic!(
      "curl script failed: stdout={} stderr={} gateway_logs={}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr),
      logs
    );
  }

  let body: Value = serde_json::from_slice(&output.stdout).expect("curl output must be JSON");
  assert_eq!(body["choices"][0]["message"]["content"], "mock response");

  let captured = mock
    .last_request()
    .expect("mock server should capture upstream request");
  assert_eq!(captured.path, "/chat/completions");
  assert_eq!(captured.header("authorization"), Some("Bearer sk-test"));
  assert_eq!(
    captured.header("user-agent"),
    Some("opencode/1.14.28 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13")
  );
  assert_eq!(captured.header("x-session-affinity"), Some("sess-opencode-e2e"));

  let payload: Value = serde_json::from_slice(&captured.body).expect("captured request body must be JSON");
  assert_eq!(payload["model"], "gpt-4o-mini");

  shutdown_gateway(&mut gateway).await;
  fs::remove_dir_all(&temp_root).unwrap();
}

fn write_config(path: &Path, port: u16) {
  fs::write(
    path,
    format!(
      "[server]\nport = {port}\nroute_mode = \"exact\"\n\n[db]\nenabled = false\n\n[logging]\nlevel = \"warn\"\ntarget = \"stderr\"\nansi = false\n",
    ),
  )
  .unwrap();
}

fn write_auth(path: &Path, base_url: &str) {
  fs::write(
    path,
    format!(
      "version: 1\naccounts:\n  - id: mock-openai\n    provider: openai\n    enabled: true\n    base_url: \"{base_url}\"\n    api_key: sk-test\n    settings: {{}}\n",
    ),
  )
  .unwrap();
}

fn spawn_gateway(config_path: &Path, xdg_config_home: PathBuf) -> Child {
  Command::new(env!("CARGO_BIN_EXE_tokn-gateway"))
    .arg("--config")
    .arg(config_path)
    .arg("serve")
    .env("XDG_CONFIG_HOME", xdg_config_home)
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn tokn-gateway")
}

async fn wait_for_gateway(url: &str) {
  let client = reqwest::Client::new();
  let deadline = std::time::Instant::now() + Duration::from_secs(15);
  loop {
    if let Ok(response) = client.get(url).send().await {
      if response.status().is_success() {
        return;
      }
    }
    assert!(
      std::time::Instant::now() < deadline,
      "gateway did not become ready in time"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
}

async fn shutdown_gateway(child: &mut Child) {
  let _ = child.start_kill();
  let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

async fn gateway_logs(child: &mut Child) -> String {
  shutdown_gateway(child).await;
  let Some(mut stderr) = child.stderr.take() else {
    return String::new();
  };
  let mut buf = Vec::new();
  let _ = stderr.read_to_end(&mut buf).await;
  String::from_utf8_lossy(&buf).into_owned()
}

fn temp_path(prefix: &str) -> PathBuf {
  let unique = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("system clock before unix epoch")
    .as_nanos();
  std::env::temp_dir().join(format!("{prefix}-{unique}"))
}

fn unused_port() -> u16 {
  TcpListener::bind("127.0.0.1:0")
    .expect("bind ephemeral port")
    .local_addr()
    .expect("read ephemeral port")
    .port()
}

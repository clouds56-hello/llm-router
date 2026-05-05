use crate::server::{self, error::ApiError, AppState};
use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::{Method, Request, Response, Uri};
use axum::response::IntoResponse;
use axum::Router;
use http::header::{HeaderValue, HOST};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;

const CA_CERT_FILE: &str = "ca.crt";
const CA_KEY_FILE: &str = "ca.key";
const CONNECT_OK: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const BAD_CONNECT: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n";
const DEFAULT_INTERCEPT_HOSTS: &[&str] = &[
  "api.openai.com",
  "api.anthropic.com",
  "api.githubcopilot.com",
  "api.z.ai",
  "open.bigmodel.cn",
  "openrouter.ai",
];

#[derive(Clone, Debug)]
pub struct ProxyOptions {
  pub addr: SocketAddr,
  pub ca_dir: PathBuf,
  pub intercept_hosts: Vec<String>,
  pub passthrough_hosts: Vec<String>,
}

pub async fn serve(state: AppState, options: ProxyOptions) -> Result<()> {
  let listener = TcpListener::bind(options.addr)
    .await
    .with_context(|| format!("bind {}", options.addr))?;
  let ca = Arc::new(load_or_generate_ca(&options.ca_dir, false)?);
  let router = proxy_router(state);
  let host_policy = HostPolicy::new(&options);

  tracing::info!(addr = %options.addr, ca_dir = %options.ca_dir.display(), "llm-router proxy listening");

  let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
  tokio::spawn(async move {
    let _ = tokio::signal::ctrl_c().await;
    let _ = shutdown_tx.send(());
  });

  loop {
    tokio::select! {
      _ = &mut shutdown_rx => break,
      accept = listener.accept() => {
        let (stream, peer) = accept?;
        let router = router.clone();
        let ca = ca.clone();
        let host_policy = host_policy.clone();
        tokio::spawn(async move {
          if let Err(err) = handle_client(stream, peer, router, ca, host_policy).await {
            tracing::warn!(%peer, error = %err, "proxy connection failed");
          }
        });
      }
    }
  }

  Ok(())
}

pub fn load_or_generate_ca(dir: &Path, force_regenerate: bool) -> Result<ProxyCa> {
  std::fs::create_dir_all(dir).with_context(|| format!("create ca dir {}", dir.display()))?;
  let cert_path = dir.join(CA_CERT_FILE);
  let key_path = dir.join(CA_KEY_FILE);

  if force_regenerate || !cert_path.exists() || !key_path.exists() {
    return generate_ca(dir);
  }

  let cert_pem = std::fs::read_to_string(&cert_path).with_context(|| format!("read {}", cert_path.display()))?;
  let key_pem = std::fs::read_to_string(&key_path).with_context(|| format!("read {}", key_path.display()))?;
  let signing_key = KeyPair::from_pem(&key_pem).context("parse CA private key")?;
  let issuer = Issuer::new(ca_params(), signing_key);
  Ok(ProxyCa {
    dir: dir.to_path_buf(),
    cert_pem,
    issuer: Arc::new(issuer),
    cert_cache: Arc::new(Mutex::new(HashMap::new())),
  })
}

fn generate_ca(dir: &Path) -> Result<ProxyCa> {
  let params = ca_params();
  let key = KeyPair::generate().context("generate CA key")?;
  let issuer = CertifiedIssuer::self_signed(params, key).context("generate CA certificate")?;

  let cert_pem = issuer.pem();
  let key_pem = issuer.key().serialize_pem();
  write_ca_file(&dir.join(CA_CERT_FILE), cert_pem.as_bytes(), 0o644)?;
  write_ca_file(&dir.join(CA_KEY_FILE), key_pem.as_bytes(), 0o600)?;

  Ok(ProxyCa {
    dir: dir.to_path_buf(),
    cert_pem,
    issuer: Arc::new(Issuer::new(ca_params(), KeyPair::from_pem(&key_pem)?)),
    cert_cache: Arc::new(Mutex::new(HashMap::new())),
  })
}

fn ca_params() -> CertificateParams {
  let mut params = CertificateParams::default();
  params.distinguished_name.push(rcgen::DnType::CommonName, "llm-router local proxy");
  params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
  params.not_before = OffsetDateTime::now_utc() - TimeDuration::days(1);
  params.not_after = OffsetDateTime::now_utc() + TimeDuration::days(3650);
  params.key_usages = vec![
    rcgen::KeyUsagePurpose::KeyCertSign,
    rcgen::KeyUsagePurpose::DigitalSignature,
    rcgen::KeyUsagePurpose::CrlSign,
  ];
  params
}

fn write_ca_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
  std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
      .with_context(|| format!("chmod {}", path.display()))?;
  }
  Ok(())
}

#[derive(Clone)]
pub struct ProxyCa {
  dir: PathBuf,
  cert_pem: String,
  issuer: Arc<Issuer<'static, KeyPair>>,
  cert_cache: Arc<Mutex<HashMap<String, Arc<CertifiedKey>>>>,
}

impl ProxyCa {
  pub fn cert_path(&self) -> PathBuf {
    self.dir.join(CA_CERT_FILE)
  }

  pub fn key_path(&self) -> PathBuf {
    self.dir.join(CA_KEY_FILE)
  }

  pub fn fingerprint_sha256(&self) -> String {
    let digest = Sha256::digest(self.cert_pem.as_bytes());
    hexify(&digest)
  }

  fn certified_key_for(&self, host: &str) -> Result<Arc<CertifiedKey>> {
    if let Some(existing) = self.cert_cache.lock().get(host).cloned() {
      return Ok(existing);
    }

    let mut params = CertificateParams::new(vec![host.to_string()]).context("build leaf certificate params")?;
    params.distinguished_name.push(rcgen::DnType::CommonName, host);
    params.not_before = OffsetDateTime::now_utc() - TimeDuration::days(1);
    params.not_after = OffsetDateTime::now_utc() + TimeDuration::days(7);
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
      rcgen::KeyUsagePurpose::DigitalSignature,
      rcgen::KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];

    let leaf_key = KeyPair::generate().context("generate leaf key")?;
    let cert = params.signed_by(&leaf_key, self.issuer.as_ref()).context("sign leaf certificate")?;
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let certified = Arc::new(
      CertifiedKey::from_der(
        vec![CertificateDer::from(cert.der().clone())],
        private_key,
        &rustls::crypto::ring::default_provider(),
      )
      .context("build rustls certified key")?,
    );
    self.cert_cache.lock().insert(host.to_string(), certified.clone());
    Ok(certified)
  }
}

impl fmt::Debug for ProxyCa {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ProxyCa")
      .field("dir", &self.dir)
      .field("cert_path", &self.cert_path())
      .field("key_path", &self.key_path())
      .field("key_pem", &"***")
      .finish()
  }
}

#[derive(Clone)]
struct HostPolicy {
  intercept: Arc<HashSet<String>>,
}

impl HostPolicy {
  fn new(options: &ProxyOptions) -> Self {
    let mut intercept = DEFAULT_INTERCEPT_HOSTS.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
    intercept.extend(options.intercept_hosts.iter().map(|s| s.to_ascii_lowercase()));
    for host in &options.passthrough_hosts {
      intercept.remove(&host.to_ascii_lowercase());
    }
    Self {
      intercept: Arc::new(intercept),
    }
  }

  fn should_intercept(&self, host: &str) -> bool {
    self.intercept.contains(&host.to_ascii_lowercase())
  }
}

async fn handle_client(
  stream: TcpStream,
  peer: SocketAddr,
  router: Router,
  ca: Arc<ProxyCa>,
  host_policy: HostPolicy,
) -> Result<()> {
  let mut reader = BufReader::new(stream);
  let mut request_line = String::new();
  if reader.read_line(&mut request_line).await? == 0 {
    return Ok(());
  }
  let request_line = request_line.trim_end_matches(['\r', '\n']);
  let mut parts = request_line.split_whitespace();
  let method = parts.next().unwrap_or_default();
  let authority = parts.next().unwrap_or_default();
  let _version = parts.next().unwrap_or_default();

  loop {
    let mut header_line = String::new();
    if reader.read_line(&mut header_line).await? == 0 {
      break;
    }
    if header_line == "\r\n" || header_line == "\n" {
      break;
    }
  }

  let mut stream = reader.into_inner();
  if method != Method::CONNECT.as_str() {
    stream.write_all(BAD_CONNECT).await?;
    return Ok(());
  }

  let (host, port) = split_authority(authority)?;
  let intercept = port == 443 && host_policy.should_intercept(&host);
  tracing::info!(%peer, host = %host, port, intercept, "proxy_connect");

  if intercept {
    stream.write_all(CONNECT_OK).await?;
    stream.flush().await?;
    intercept_tls(stream, &host, router, ca).await
  } else {
    tunnel(stream, &host, port).await
  }
}

async fn tunnel(mut client: TcpStream, host: &str, port: u16) -> Result<()> {
  let mut upstream = TcpStream::connect((host, port))
    .await
    .with_context(|| format!("connect upstream {host}:{port}"))?;
  client.write_all(CONNECT_OK).await?;
  client.flush().await?;
  tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
  Ok(())
}

async fn intercept_tls(stream: TcpStream, host: &str, router: Router, ca: Arc<ProxyCa>) -> Result<()> {
  let resolver = Arc::new(DynamicResolver {
    ca,
    fallback_host: host.to_string(),
  });
  let tls = TlsAcceptor::from(Arc::new(
    rustls::ServerConfig::builder()
      .with_no_client_auth()
      .with_cert_resolver(resolver),
  ));
  let tls_stream = tls.accept(stream).await.context("TLS handshake failed")?;
  let mut http = http1::Builder::new();
  http.keep_alive(true).title_case_headers(true);

  let service = service_fn(move |req| route_intercepted_request(router.clone(), req));
  http
    .serve_connection(TokioIo::new(tls_stream), service)
    .await
    .context("serve intercepted HTTP connection")?;
  Ok(())
}

async fn route_intercepted_request(router: Router, req: Request<hyper::body::Incoming>) -> Result<Response<Body>, std::convert::Infallible> {
  let host = req
    .headers()
    .get(HOST)
    .and_then(|v| v.to_str().ok())
    .map(|s| s.split(':').next().unwrap_or(s).to_string())
    .unwrap_or_default();
  let path = req.uri().path().to_string();
  let method = req.method().clone();

  let rewritten = if let Some(rewritten) = rewrite_target(&host, &path, &method) {
    rewritten
  } else {
    return Ok(ApiError::not_implemented(path, host).into_response());
  };

  let path_and_query = req
    .uri()
    .path_and_query()
    .map(|v| v.as_str())
    .unwrap_or(&path);
  let rewritten_path_and_query = path_and_query.replacen(&path, rewritten, 1);
  let uri = Uri::builder()
    .path_and_query(rewritten_path_and_query.as_str())
    .build()
    .unwrap_or_else(|_| Uri::from_static("/"));

  let (parts, body) = req.into_parts();
  let mut builder = Request::builder().method(method).uri(uri).version(parts.version);
  for (key, value) in &parts.headers {
    if key != HOST {
      builder = builder.header(key, value);
    }
  };
  builder = builder.header(HOST, HeaderValue::from_str(&host).unwrap_or_else(|_| HeaderValue::from_static("localhost")));
  let body = Body::new(body);
  let request = builder.body(body).unwrap_or_else(|_| Request::new(Body::empty()));

  use tower::ServiceExt;
  let response = router.oneshot(request).await.unwrap_or_else(|err| ApiError::bad_gateway(err.to_string()).into_response());
  Ok(response)
}

fn rewrite_target(host: &str, path: &str, method: &Method) -> Option<&'static str> {
  match (host, method, path) {
    (_, &Method::GET, "/v1/models") => Some("/v1/models"),
    ("api.openai.com", &Method::POST, "/v1/chat/completions") => Some("/v1/chat/completions"),
    ("api.openai.com", &Method::POST, "/v1/responses") => Some("/v1/responses"),
    ("api.anthropic.com", &Method::POST, "/v1/messages") => Some("/v1/messages"),
    ("api.githubcopilot.com", &Method::POST, "/v1/chat/completions") => Some("/v1/chat/completions"),
    ("api.z.ai", &Method::POST, "/v1/chat/completions") => Some("/v1/chat/completions"),
    ("open.bigmodel.cn", &Method::POST, "/api/paas/v4/chat/completions") => Some("/v1/chat/completions"),
    ("openrouter.ai", &Method::POST, "/api/v1/chat/completions") => Some("/v1/chat/completions"),
    _ => None,
  }
}

fn proxy_router(state: AppState) -> Router {
  server::router(state)
}

fn split_authority(authority: &str) -> Result<(String, u16)> {
  let (host, port) = authority
    .rsplit_once(':')
    .with_context(|| format!("invalid CONNECT authority '{authority}'"))?;
  Ok((host.to_ascii_lowercase(), port.parse().with_context(|| format!("invalid CONNECT port in '{authority}'"))?))
}

fn hexify(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len() * 2);
  for b in bytes {
    use std::fmt::Write as _;
    let _ = write!(out, "{b:02x}");
  }
  out
}

#[derive(Debug)]
struct DynamicResolver {
  ca: Arc<ProxyCa>,
  fallback_host: String,
}

impl ResolvesServerCert for DynamicResolver {
  fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
    let host = client_hello.server_name().unwrap_or(&self.fallback_host);
    self.ca.certified_key_for(host).ok()
  }
}

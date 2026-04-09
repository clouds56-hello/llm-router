use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Instant};

use adapter_claude::ClaudeAdapter;
use adapter_codex::CodexAdapter;
use adapter_copilot::CopilotAdapter;
use adapter_openai::OpenAiAdapter;
use anyhow::Context;
use axum::{
  extract::State,
  http::{HeaderMap, StatusCode},
  response::{sse::Event, IntoResponse, Response, Sse},
  routing::{get, post},
  Json, Router,
};
use futures::StreamExt;
use notify::{RecursiveMode, Watcher};
use plugin_api::{EventRecord, Plugin, PluginManager};
use plugin_defaults::{JsonLoggerPlugin, MetricsPlugin, SqliteSinkPlugin, SqliteStore};
use router_core::{
  AdapterRegistry, OpenAiChatCompletionRequest, OpenAiEmbeddingRequest, OpenAiErrorEnvelope, ProviderKind,
  RequestContext, ResolvedConfig, RouterConfig, RouterEngine, RouterError,
};
use tokio::{sync::RwLock, time::Duration};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};
use viewer_web::ViewerState;

#[derive(Clone)]
struct RuntimeState {
  engine: Arc<RouterEngine>,
}

#[derive(Clone)]
struct AppState {
  runtime: Arc<RwLock<RuntimeState>>,
  plugins: PluginManager,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,llm_routerd=debug".into()),
    )
    .init();

  let config_path = std::env::args()
    .nth(1)
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("router.yaml"));

  let config = load_config(&config_path)?;
  let engine = build_engine(config.clone())?;

  let store = SqliteStore::new(
    &config.viewer.sqlite_path,
    config.viewer.max_rows,
    config.viewer.max_age_seconds,
  )?;

  let plugins: Vec<Arc<dyn Plugin>> = vec![
    Arc::new(JsonLoggerPlugin),
    Arc::new(MetricsPlugin::new()),
    Arc::new(SqliteSinkPlugin::new(store.clone())),
  ];
  let plugin_manager = PluginManager::new(plugins, 2048);

  let runtime = Arc::new(RwLock::new(RuntimeState {
    engine: Arc::new(engine),
  }));

  let state = AppState {
    runtime: runtime.clone(),
    plugins: plugin_manager,
  };

  spawn_hot_reload(config_path.clone(), runtime.clone());
  spawn_retention_pruner(store.clone());

  let mut app = Router::new()
    .route("/healthz", get(healthz))
    .route("/v1/models", get(list_models))
    .route("/v1/chat/completions", post(chat_completions))
    .route("/v1/embeddings", post(embeddings))
    .layer(CorsLayer::permissive())
    .layer(TraceLayer::new_for_http())
    .with_state(state);

  if config.viewer.enabled {
    app = app.nest(
      &config.viewer.path_prefix,
      viewer_web::router(ViewerState { store: store.clone() }),
    );
  }

  let addr: SocketAddr = config.listener.bind.parse().context("invalid listener.bind")?;
  info!("llm-router listening on {}", addr);
  let listener = tokio::net::TcpListener::bind(addr).await?;
  axum::serve(listener, app).await?;

  Ok(())
}

fn load_config(path: &PathBuf) -> anyhow::Result<RouterConfig> {
  let raw = std::fs::read_to_string(path).with_context(|| format!("read config failed: {}", path.display()))?;
  let cfg: RouterConfig =
    serde_yaml::from_str(&raw).with_context(|| format!("parse yaml failed: {}", path.display()))?;
  Ok(cfg)
}

fn build_engine(config: RouterConfig) -> anyhow::Result<RouterEngine> {
  let resolved = config
    .validate_and_resolve()
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
  let registry = build_registry(&resolved);
  Ok(RouterEngine::new(resolved, Arc::new(registry)))
}

fn build_registry(resolved: &ResolvedConfig) -> AdapterRegistry {
  let mut reg = AdapterRegistry::new();
  for (provider_name, rp) in &resolved.providers {
    let adapter: Arc<dyn router_core::ProviderAdapter> = match rp.config.kind {
      ProviderKind::OpenAi => Arc::new(OpenAiAdapter::new(
        provider_name.clone(),
        rp.config.base_url.clone(),
        rp.api_key.clone(),
      )),
      ProviderKind::Claude => Arc::new(ClaudeAdapter::new(
        provider_name.clone(),
        rp.config.base_url.clone(),
        rp.api_key.clone(),
      )),
      ProviderKind::Copilot => Arc::new(CopilotAdapter::new(
        provider_name.clone(),
        rp.config.base_url.clone(),
        rp.api_key.clone(),
      )),
      ProviderKind::Codex => Arc::new(CodexAdapter::new(
        provider_name.clone(),
        rp.config.base_url.clone(),
        rp.api_key.clone(),
      )),
    };
    reg.register(provider_name.clone(), adapter);
  }
  reg
}

fn spawn_hot_reload(path: PathBuf, runtime: Arc<RwLock<RuntimeState>>) {
  tokio::spawn(async move {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut watcher = match notify::recommended_watcher(move |res| {
      let _ = tx.blocking_send(res);
    }) {
      Ok(w) => w,
      Err(err) => {
        error!("file watcher init failed: {}", err);
        return;
      }
    };

    if let Err(err) = watcher.watch(&path, RecursiveMode::NonRecursive) {
      error!("watch config failed ({}): {}", path.display(), err);
      return;
    }

    while let Some(event) = rx.recv().await {
      match event {
        Ok(_evt) => match load_config(&path).and_then(build_engine) {
          Ok(new_engine) => {
            let mut guard = runtime.write().await;
            guard.engine = Arc::new(new_engine);
            info!("hot reload applied from {}", path.display());
          }
          Err(err) => {
            warn!("hot reload rejected ({}): {}", path.display(), err);
          }
        },
        Err(err) => warn!("watch error: {}", err),
      }
    }
  });
}

fn spawn_retention_pruner(store: SqliteStore) {
  tokio::spawn(async move {
    loop {
      tokio::time::sleep(Duration::from_secs(60)).await;
      if let Err(err) = store.prune() {
        warn!("retention prune failed: {}", err);
      }
    }
  });
}

async fn healthz() -> impl IntoResponse {
  Json(serde_json::json!({"ok": true}))
}

async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
  let ctx = RequestContext::new(extract_api_key(&headers));
  let engine = state.runtime.read().await.engine.clone();
  match engine.list_models(&ctx).await {
    Ok(models) => Json(models).into_response(),
    Err(err) => error_response(err),
  }
}

async fn embeddings(
  State(state): State<AppState>,
  headers: HeaderMap,
  Json(req): Json<OpenAiEmbeddingRequest>,
) -> Response {
  let mut ctx = RequestContext::new(extract_api_key(&headers));
  let started = Instant::now();
  let engine = state.runtime.read().await.engine.clone();

  let synthetic_chat_req = OpenAiChatCompletionRequest {
    model: req.model.clone(),
    messages: vec![],
    stream: false,
    extra: Default::default(),
  };
  state.plugins.emit(EventRecord::RequestStart {
    ctx: ctx.clone(),
    req: synthetic_chat_req,
  });

  match engine.route_embeddings(&mut ctx, &req).await {
    Ok(resp) => {
      state.plugins.emit(EventRecord::RequestEnd {
        ctx,
        status_code: 200,
        latency_ms: started.elapsed().as_millis(),
      });
      Json(resp).into_response()
    }
    Err(err) => {
      state.plugins.emit(EventRecord::RequestError {
        ctx,
        error: err.clone(),
        latency_ms: started.elapsed().as_millis(),
      });
      error_response(err)
    }
  }
}

async fn chat_completions(
  State(state): State<AppState>,
  headers: HeaderMap,
  Json(req): Json<OpenAiChatCompletionRequest>,
) -> Response {
  let mut ctx = RequestContext::new(extract_api_key(&headers));
  let started = Instant::now();
  let engine = state.runtime.read().await.engine.clone();

  state.plugins.emit(EventRecord::RequestStart {
    ctx: ctx.clone(),
    req: req.clone(),
  });

  if req.stream {
    match engine.route_chat_stream(&mut ctx, &req).await {
      Ok(stream) => {
        let plugins = state.plugins.clone();
        let ctx_copy = ctx.clone();
        let stream = stream.map(move |item| match item {
          Ok(chunk) => {
            plugins.emit(EventRecord::StreamChunk {
              ctx: ctx_copy.clone(),
              chunk: chunk.clone(),
            });
            let data = serde_json::to_string(&chunk).unwrap_or_else(|_| "{}".to_string());
            Ok::<Event, std::convert::Infallible>(Event::default().data(data))
          }
          Err(_) => Ok(Event::default().data("[DONE]")),
        });

        state.plugins.emit(EventRecord::RequestEnd {
          ctx,
          status_code: 200,
          latency_ms: started.elapsed().as_millis(),
        });
        return Sse::new(stream).into_response();
      }
      Err(err) => {
        state.plugins.emit(EventRecord::RequestError {
          ctx,
          error: err.clone(),
          latency_ms: started.elapsed().as_millis(),
        });
        return error_response(err);
      }
    }
  }

  match engine.route_chat(&mut ctx, &req).await {
    Ok(resp) => {
      state.plugins.emit(EventRecord::RequestEnd {
        ctx,
        status_code: 200,
        latency_ms: started.elapsed().as_millis(),
      });
      Json(resp).into_response()
    }
    Err(err) => {
      state.plugins.emit(EventRecord::RequestError {
        ctx,
        error: err.clone(),
        latency_ms: started.elapsed().as_millis(),
      });
      error_response(err)
    }
  }
}

fn extract_api_key(headers: &HeaderMap) -> Option<String> {
  headers
    .get("authorization")
    .and_then(|v| v.to_str().ok())
    .map(ToString::to_string)
}

fn error_response(err: RouterError) -> Response {
  let status = StatusCode::from_u16(err.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
  let body: OpenAiErrorEnvelope = err.as_openai_error();
  (status, Json(body)).into_response()
}

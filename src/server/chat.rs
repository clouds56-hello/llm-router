use super::error::ApiError;
use super::AppState;
use crate::copilot::{self, COPILOT_API};
use crate::pool::Account;
use crate::usage::Record;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

const MAX_RETRIES: usize = 2;

pub async fn chat_completions(
    State(s): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let started = Instant::now();
    let mut last_err: Option<(StatusCode, String)> = None;

    for attempt in 0..=MAX_RETRIES {
        let acct = s.pool.acquire();
        let token = match acct.ensure_api_token(&s.http).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(account = %acct.id, attempt, error = %e, "token refresh failed");
                acct.mark_failure(s.pool.cooldown_base());
                last_err = Some((StatusCode::BAD_GATEWAY, e.to_string()));
                continue;
            }
        };

        let headers = match copilot::headers::copilot_request_headers(&token, &acct.headers, stream) {
            Ok(h) => h,
            Err(e) => return Err(ApiError::internal(e.to_string())),
        };

        let url = format!("{COPILOT_API}/chat/completions");
        let resp = match s.http.post(&url).headers(headers).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(account = %acct.id, attempt, error = %e, "upstream send failed");
                acct.mark_failure(s.pool.cooldown_base());
                last_err = Some((StatusCode::BAD_GATEWAY, e.to_string()));
                continue;
            }
        };

        let status = resp.status();

        // Retryable conditions
        if status == StatusCode::UNAUTHORIZED {
            tracing::warn!(account = %acct.id, attempt, "401 from upstream; refreshing token");
            acct.invalidate_api_token();
            // do not cooldown on 401 — token may simply be stale
            last_err = Some((status, "unauthorized".into()));
            continue;
        }
        if status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::FORBIDDEN
            || status.is_server_error()
        {
            let body_text = resp.text().await.unwrap_or_default();
            tracing::warn!(account = %acct.id, attempt, %status, body = %body_text, "upstream error; cooldown");
            acct.mark_failure(s.pool.cooldown_base());
            last_err = Some((status, body_text));
            continue;
        }

        // Success path — handover to streaming or buffered handler.
        acct.mark_success();

        if stream {
            return Ok(stream_response(s.clone(), acct, resp, model, started).await);
        } else {
            return Ok(buffered_response(s.clone(), acct, resp, model, started).await);
        }
    }

    let (status, msg) = last_err.unwrap_or((StatusCode::BAD_GATEWAY, "all attempts failed".into()));
    Err(ApiError::upstream(status, msg))
}

async fn buffered_response(
    s: AppState,
    acct: Arc<Account>,
    resp: reqwest::Response,
    model: String,
    started: Instant,
) -> Response {
    let status = resp.status();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return ApiError::upstream(StatusCode::BAD_GATEWAY, e.to_string()).into_response();
        }
    };

    // Try to extract token usage from JSON body.
    let (pt, ct) = parse_usage_from_json(&bytes);
    record_usage(&s, &acct.id, &model, pt, ct, started, status.as_u16(), false);

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    (status, headers, bytes).into_response()
}

async fn stream_response(
    s: AppState,
    acct: Arc<Account>,
    resp: reqwest::Response,
    model: String,
    started: Instant,
) -> Response {
    let status = resp.status();

    // Capture last data line containing "usage" for accounting.
    let usage_holder = Arc::new(parking_lot::Mutex::new((None::<u64>, None::<u64>)));
    let usage_for_stream = usage_holder.clone();
    let acct_id = acct.id.clone();
    let model_clone = model.clone();
    let s_clone = s.clone();

    let upstream = resp.bytes_stream();
    let mut buffer = Vec::<u8>::new();

    let mapped = upstream.map(move |chunk| match chunk {
        Ok(b) => {
            // Scan for SSE "data: {...}" lines and try to extract usage.
            buffer.extend_from_slice(&b);
            while let Some(pos) = buffer.iter().position(|&c| c == b'\n') {
                let line: Vec<u8> = buffer.drain(..=pos).collect();
                let s = String::from_utf8_lossy(&line);
                let trimmed = s.trim_start();
                if let Some(rest) = trimmed.strip_prefix("data:") {
                    let payload = rest.trim();
                    if payload.is_empty() || payload == "[DONE]" {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(payload) {
                        if let Some(u) = v.get("usage") {
                            let pt = u.get("prompt_tokens").and_then(|x| x.as_u64());
                            let ct = u.get("completion_tokens").and_then(|x| x.as_u64());
                            if pt.is_some() || ct.is_some() {
                                *usage_for_stream.lock() = (pt, ct);
                            }
                        }
                    }
                }
            }
            Ok::<Bytes, std::io::Error>(b)
        }
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    });

    // Record at the end: spawn a task that drains a oneshot when stream ends.
    // Simpler: wrap the stream so it records on Drop.
    let recorded = Arc::new(parking_lot::Mutex::new(false));
    let recorded_clone = recorded.clone();
    let on_end = move || {
        if *recorded_clone.lock() {
            return;
        }
        *recorded_clone.lock() = true;
        let (pt, ct) = *usage_holder.lock();
        record_usage(&s_clone, &acct_id, &model_clone, pt, ct, started, status.as_u16(), true);
    };

    let stream = StreamWithFinalizer::new(mapped, on_end);
    let body = Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    headers.insert(
        axum::http::header::CONNECTION,
        HeaderValue::from_static("keep-alive"),
    );
    (status, headers, body).into_response()
}

fn parse_usage_from_json(bytes: &[u8]) -> (Option<u64>, Option<u64>) {
    let v: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let u = match v.get("usage") {
        Some(u) => u,
        None => return (None, None),
    };
    let pt = u.get("prompt_tokens").and_then(|x| x.as_u64());
    let ct = u.get("completion_tokens").and_then(|x| x.as_u64());
    (pt, ct)
}

fn record_usage(
    s: &AppState,
    account_id: &str,
    model: &str,
    pt: Option<u64>,
    ct: Option<u64>,
    started: Instant,
    status: u16,
    stream: bool,
) {
    if !s.usage_enabled {
        return;
    }
    let Some(db) = s.usage.as_ref() else { return };
    let latency_ms = started.elapsed().as_millis() as u64;
    if let Err(e) = db.record(Record {
        account_id,
        model,
        prompt_tokens: pt,
        completion_tokens: ct,
        latency_ms,
        status,
        stream,
    }) {
        tracing::warn!(error = %e, "failed to write usage row");
    }
}

// --- Stream wrapper that runs a closure when polled to completion or dropped.

use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

struct StreamWithFinalizer<S, F: FnOnce() + Send + 'static> {
    inner: S,
    fin: Option<F>,
}

impl<S, F: FnOnce() + Send + 'static> StreamWithFinalizer<S, F> {
    fn new(inner: S, f: F) -> Self {
        Self { inner, fin: Some(f) }
    }
}

impl<S, F> Stream for StreamWithFinalizer<S, F>
where
    S: Stream + Unpin,
    F: FnOnce() + Send + 'static + Unpin,
{
    type Item = S::Item;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let p = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(None) = &p {
            if let Some(f) = self.fin.take() {
                f();
            }
        }
        p
    }
}

impl<S, F: FnOnce() + Send + 'static> Drop for StreamWithFinalizer<S, F> {
    fn drop(&mut self) {
        if let Some(f) = self.fin.take() {
            f();
        }
    }
}

//! Requests stage-event persistence.
//!
//! Subscribes to `Event::Requests(RequestEvent { request_id, attempt, payload })`
//! and writes one row per `(request_id, attempt)` into the per-day
//! `requests/<YYYY-MM-DD>.db` files. Mirrors the incremental pattern of
//! the legacy lifecycle writer ([`super::legacy`]): a single INSERT in
//! [`RequestEventHandler::on_started`] and one UPDATE per subsequent stage.
//!
//! `RequestEventHandler` owns the stage-event persistence semantics while
//! [`RequestsDb`] stays the low-level day-rotated connection cache and
//! `request_id → day` index used to route updates to the correct file.

use super::{composite_request_id, now_unix, RequestsDb};
use crate::{headers_json, Result};
use llm_core::event::{Event, EventHandler};
use llm_core::request_event::{RecordEvent, RequestEventPayload, Stage, StageEvent};
use rusqlite::params;
use std::path::PathBuf;

/// `EventHandler` that persists requests stage events into the requests DB.
/// Construct once and register alongside the legacy `DbEventHandler` —
/// both run in the same `spawn_event_loop` and each maintains its own
/// per-day connection cache.
pub struct RequestEventHandler {
  db: RequestsDb,
}

impl RequestEventHandler {
  pub fn new(requests_dir: PathBuf) -> Result<Self> {
    Ok(Self {
      db: RequestsDb::new(requests_dir)?,
    })
  }
}

impl EventHandler for RequestEventHandler {
  fn handle(&mut self, event: &Event) {
    let Event::Requests(r2) = event else {
      return;
    };
    let request_id = r2.request_id.as_str();
    let attempt = r2.attempt;
    let result = match &r2.payload {
      RequestEventPayload::Custom(_) => return,
      RequestEventPayload::Stage(stage) => match stage {
        StageEvent::Started { endpoint } => self.on_started(request_id, attempt, now_unix(), endpoint.as_str()),
        StageEvent::Extract(s) => self.on_extract(
          request_id,
          attempt,
          s.model.as_str(),
          s.stream,
          s.session_id.as_deref(),
          s.initiator.as_str(),
          &s.headers,
          &s.raw_body,
        ),
        StageEvent::Resolve(s) => self.on_resolve(
          request_id,
          attempt,
          s.account_id.as_str(),
          s.provider_id.as_str(),
          s.upstream_endpoint.as_str(),
        ),
        StageEvent::BuildHeaders(s) => self.on_build_headers(request_id, attempt, &s.headers),
        StageEvent::ConvertRequest(s) => self.on_convert_request(request_id, attempt, &s.upstream_wire_body),
        StageEvent::Send(s) => self.on_send(request_id, attempt, now_unix(), s.status, &s.headers),
        StageEvent::ConvertResponse(s) => {
          let body_bytes = s
            .body
            .as_ref()
            .map(|v| bytes::Bytes::from(serde_json::to_vec(v.as_ref()).unwrap_or_default()))
            .unwrap_or_default();
          self.on_convert_response(request_id, attempt, s.status, &s.headers, &body_bytes)
        }
        StageEvent::Error { stage, message, .. } => self.on_error(request_id, attempt, *stage, message.as_str()),
        StageEvent::Completed { .. } => self.on_completed(request_id, attempt, now_unix()),
      },
      // Record events capture transport-adjacent facts that live alongside
      // the stage lifecycle. `UpstreamResp` duplicates `StageEvent::Send`
      // (status + headers come from the same `reqwest::Response::headers()`)
      // and so is intentionally a no-op here.
      RequestEventPayload::Record(RecordEvent::InboundConnection {
        local_addr,
        peer_addr,
        mode,
        method,
        url,
      }) => self.on_inbound_connection(
        request_id,
        attempt,
        local_addr.as_deref(),
        peer_addr.as_deref(),
        mode.as_str(),
        method.as_str(),
        url.as_deref(),
      ),
      RequestEventPayload::Record(RecordEvent::UpstreamReq {
        method,
        url,
        headers,
        body,
      }) => self.on_upstream_req(request_id, attempt, method.as_str(), url.as_str(), headers, body),
      RequestEventPayload::Record(RecordEvent::UpstreamResp { .. }) => Ok(()),
      RequestEventPayload::Record(RecordEvent::UpstreamBody { body }) => self.on_upstream_body(request_id, attempt, body),
      RequestEventPayload::Record(RecordEvent::Usage(usage)) => self.on_usage(request_id, attempt, usage),
    };
    if let Err(e) = result {
      tracing::warn!(error = %e, request_id, attempt, "requests persistence write failed");
    }
  }
}

impl RequestEventHandler {
  pub fn on_inbound_connection(
    &mut self,
    request_id: &str,
    attempt: u32,
    local_addr: Option<&str>,
    peer_addr: Option<&str>,
    mode: &str,
    method: &str,
    url: Option<&str>,
  ) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests InboundConnection without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET
         local_addr = COALESCE(?2, local_addr),
         peer_addr = COALESCE(?3, peer_addr),
         mode = COALESCE(?4, mode),
         method = COALESCE(?5, method),
         inbound_req_method = COALESCE(?5, inbound_req_method),
         inbound_req_url = COALESCE(?6, inbound_req_url)
       WHERE request_id = ?1",
      params![id, local_addr, peer_addr, mode, method, url],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests InboundConnection UPDATE matched no row");
    }
    Ok(())
  }

  /// Single INSERT for a fresh request. All subsequent stage handlers are
  /// UPDATEs that assume this row exists.
  pub fn on_started(&mut self, request_id: &str, attempt: u32, ts: i64, endpoint: &str) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let conn = self.db.conn_for_ts(ts)?;
    conn.execute(
      "INSERT INTO requests (request_id, ts, endpoint, account_id, provider_id, model, initiator)
       VALUES (?1, ?2, ?3, '', '', '', '')
       ON CONFLICT(request_id) DO NOTHING",
      params![id, ts, endpoint],
    )?;
    self.db.pin_request_day(&id, ts);
    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  pub fn on_extract(
    &mut self,
    request_id: &str,
    attempt: u32,
    model: &str,
    stream: bool,
    session_id: Option<&str>,
    initiator: &str,
    inbound_req_headers: &llm_headers::HeaderMap,
    inbound_req_body: &bytes::Bytes,
  ) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let hdr_json = headers_json(inbound_req_headers);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests Extract without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET
         model = ?2,
         stream = ?3,
         session_id = COALESCE(?4, session_id),
         initiator = ?5,
         inbound_req_headers = ?6,
         inbound_req_body = ?7
       WHERE request_id = ?1",
      params![
        id,
        model,
        stream as i64,
        session_id,
        initiator,
        hdr_json.as_ref(),
        inbound_req_body.as_ref()
      ],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests Extract UPDATE matched no row");
    }
    Ok(())
  }

  pub fn on_resolve(
    &mut self,
    request_id: &str,
    attempt: u32,
    account_id: &str,
    provider_id: &str,
    upstream_endpoint: &str,
  ) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests Resolve without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET
         account_id = ?2,
         provider_id = ?3,
         endpoint = ?4
       WHERE request_id = ?1",
      params![id, account_id, provider_id, upstream_endpoint],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests Resolve UPDATE matched no row");
    }
    Ok(())
  }

  pub fn on_build_headers(
    &mut self,
    request_id: &str,
    attempt: u32,
    outbound_req_headers: &llm_headers::HeaderMap,
  ) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let hdr_json = headers_json(outbound_req_headers);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests BuildHeaders without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET outbound_req_headers = ?2 WHERE request_id = ?1",
      params![id, hdr_json.as_ref()],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests BuildHeaders UPDATE matched no row");
    }
    Ok(())
  }

  pub fn on_convert_request(&mut self, request_id: &str, attempt: u32, outbound_req_body: &bytes::Bytes) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests ConvertRequest without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET outbound_req_body = ?2 WHERE request_id = ?1",
      params![id, outbound_req_body.as_ref()],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests ConvertRequest UPDATE matched no row");
    }
    Ok(())
  }

  pub fn on_send(
    &mut self,
    request_id: &str,
    attempt: u32,
    ts_now: i64,
    status: u16,
    outbound_resp_headers: &llm_headers::HeaderMap,
  ) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let hdr_json = headers_json(outbound_resp_headers);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests Send without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET
         outbound_resp_status = ?2,
         outbound_resp_headers = ?3,
         status = ?2,
         latency_header_ms = (?4 - ts) * 1000
       WHERE request_id = ?1",
      params![id, status as i64, hdr_json.as_ref(), ts_now],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests Send UPDATE matched no row");
    }
    Ok(())
  }

  pub fn on_convert_response(
    &mut self,
    request_id: &str,
    attempt: u32,
    status: u16,
    inbound_resp_headers: &llm_headers::HeaderMap,
    inbound_resp_body: &bytes::Bytes,
  ) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let hdr_json = headers_json(inbound_resp_headers);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests ConvertResponse without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET
         inbound_resp_status = ?2,
         inbound_resp_headers = ?3,
         inbound_resp_body = ?4
       WHERE request_id = ?1",
      params![id, status as i64, hdr_json.as_ref(), inbound_resp_body.as_ref()],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests ConvertResponse UPDATE matched no row");
    }
    Ok(())
  }

  pub fn on_error(&mut self, request_id: &str, attempt: u32, stage: Stage, message: &str) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let formatted = format!("{}: {message}", stage.as_str());
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests Error without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET request_error = ?2 WHERE request_id = ?1",
      params![id, formatted],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests Error UPDATE matched no row");
    }
    Ok(())
  }

  pub fn on_completed(&mut self, request_id: &str, attempt: u32, ts_now: i64) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests Completed without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET latency_ms = (?2 - ts) * 1000 WHERE request_id = ?1",
      params![id, ts_now],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests Completed UPDATE matched no row");
    }
    Ok(())
  }

  /// Wire-truth upstream-request record. Overwrites the intent-time values
  /// written by `on_build_headers` / `on_convert_request` with what
  /// actually went on the wire (post auth injection, post Host /
  /// Content-Length strip, post body-bytes finalization). Also fills the
  /// previously-empty `outbound_req_method` and `outbound_req_url`
  /// columns that no stage event populated before `Record::UpstreamReq`
  /// existed.
  pub fn on_upstream_req(
    &mut self,
    request_id: &str,
    attempt: u32,
    method: &str,
    url: &str,
    headers: &llm_headers::HeaderMap,
    body: &bytes::Bytes,
  ) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let hdr_json = headers_json(headers);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests UpstreamReq without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET
         outbound_req_method = ?2,
         outbound_req_url = ?3,
         outbound_req_headers = ?4,
         outbound_req_body = ?5
       WHERE request_id = ?1",
      params![id, method, url, hdr_json.as_ref(), body.as_ref()],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests UpstreamReq UPDATE matched no row");
    }
    Ok(())
  }

  /// Wire-truth upstream-response body. Written by ConvertResponse for
  /// buffered flows; streaming responses are not captured here (the live
  /// SSE byte stream is single-shot and can't be cheaply tee'd, matching
  /// legacy behavior). The `Send` stage already wrote status + response
  /// headers, so this update touches only the body column.
  pub fn on_upstream_body(&mut self, request_id: &str, attempt: u32, body: &bytes::Bytes) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests UpstreamBody without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET outbound_resp_body = ?2 WHERE request_id = ?1",
      params![id, body.as_ref()],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests UpstreamBody UPDATE matched no row");
    }
    Ok(())
  }

  pub fn on_usage(&mut self, request_id: &str, attempt: u32, usage: &llm_core::db::Usage) -> Result<()> {
    let id = composite_request_id(request_id, attempt);
    let Some(conn) = self.db.conn_for_request(&id) else {
      tracing::warn!(request_id = %id, "requests Usage without prior Started");
      return Ok(());
    };
    let updated = conn.execute(
      "UPDATE requests SET
         input_tok = ?2,
         output_tok = ?3,
         cached_tok = ?4,
         reasoning_tok = ?5
       WHERE request_id = ?1",
      params![
        id,
        usage.input_tokens.map(|v| v as i64),
        usage.output_tokens.map(|v| v as i64),
        usage.details.cache_read.map(|v| v as i64),
        usage.details.reasoning.map(|v| v as i64),
      ],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %id, "requests Usage UPDATE matched no row");
    }
    Ok(())
  }
}

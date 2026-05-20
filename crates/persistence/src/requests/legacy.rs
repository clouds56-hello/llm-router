//! Legacy per-request lifecycle writer.
//!
//! Extends [`crate::DbEventHandler`] with the lifecycle methods driven by
//! `LegacyRequestEvent::*`. [`RequestsDb`] stays the low-level day-file
//! cache/lookup layer; all legacy request-write semantics live here.

use super::{composite_request_id, RequestsDb};
use crate::{headers_json, write_record, CallRecord, DbEventHandler, HttpSnapshot, PendingRequest, Result, SessionSource, Usage};
use rusqlite::params;

/// Header-time update payload used by [`DbEventHandler::on_headers`].
pub struct HeadersUpdate<'a> {
  pub ts: i64,
  pub endpoint: &'a str,
  pub session_id: Option<&'a str>,
  pub local_addr: Option<&'a str>,
  pub mode: Option<&'a str>,
  pub method: Option<&'a str>,
  pub inbound_req: &'a HttpSnapshot,
}

/// Optional contextual fields written alongside [`DbEventHandler::on_started`].
#[derive(Default)]
pub struct RequestContext<'a> {
  pub user: Option<&'a str>,
  pub local_addr: Option<&'a str>,
  pub mode: Option<&'a str>,
  pub behave_as: Option<&'a str>,
}

/// Parse-time update payload used by [`DbEventHandler::on_parsed`].
pub struct ParsedUpdate<'a> {
  pub ts: i64,
  pub endpoint: &'a str,
  pub account_id: &'a str,
  pub provider_id: &'a str,
  pub model: &'a str,
  pub initiator: &'a str,
  pub stream: bool,
  pub behave_as: Option<&'a str>,
  pub inbound_body: bytes::Bytes,
}

fn opt_str<'a, F>(snap: Option<&'a HttpSnapshot>, f: F) -> Option<&'a str>
where
  F: FnOnce(&'a HttpSnapshot) -> Option<&'a str>,
{
  snap.and_then(f)
}

fn persist_record(db: &mut RequestsDb, r: &CallRecord) -> Result<()> {
  let conn = db.conn_for_ts(r.ts)?;
  let outbound_resp_headers = r.outbound.as_ref().map(|s| headers_json(&s.resp_headers));
  let inbound_resp_headers = headers_json(&r.inbound.resp_headers);
  conn.execute(
    "INSERT INTO requests (ts, session_id, user, local_addr, mode, behave_as, peer_addr, method, request_id, request_error, endpoint, account_id, provider_id,
                            model, initiator, status, stream, latency_ms, latency_header_ms,
                            input_tok, output_tok, cached_tok, reasoning_tok,
                           inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body,
                           outbound_req_method, outbound_req_url, outbound_req_headers, outbound_req_body,
                           outbound_resp_status, outbound_resp_headers, outbound_resp_body,
                           inbound_resp_status, inbound_resp_headers, inbound_resp_body)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20,
             ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37)
     ON CONFLICT(request_id) DO UPDATE SET
       user=COALESCE(user, excluded.user),
       local_addr=COALESCE(local_addr, excluded.local_addr),
       mode=COALESCE(mode, excluded.mode),
       behave_as=COALESCE(behave_as, excluded.behave_as),
       peer_addr=COALESCE(peer_addr, excluded.peer_addr),
       method=COALESCE(method, excluded.method),
       request_error=excluded.request_error,
       endpoint=excluded.endpoint,
       account_id=excluded.account_id,
       provider_id=excluded.provider_id,
       model=excluded.model,
       initiator=excluded.initiator,
       status=excluded.status,
       stream=excluded.stream,
       latency_ms=excluded.latency_ms,
       latency_header_ms=COALESCE(latency_header_ms, excluded.latency_header_ms),
       input_tok=excluded.input_tok,
       output_tok=excluded.output_tok,
       cached_tok=excluded.cached_tok,
       reasoning_tok=excluded.reasoning_tok,
       inbound_req_method=excluded.inbound_req_method,
       inbound_req_url=excluded.inbound_req_url,
       inbound_req_headers=excluded.inbound_req_headers,
       inbound_req_body=excluded.inbound_req_body,
       outbound_req_method=excluded.outbound_req_method,
       outbound_req_url=excluded.outbound_req_url,
       outbound_req_headers=excluded.outbound_req_headers,
       outbound_req_body=excluded.outbound_req_body,
       outbound_resp_status=excluded.outbound_resp_status,
       outbound_resp_headers=excluded.outbound_resp_headers,
       outbound_resp_body=excluded.outbound_resp_body,
       inbound_resp_status=excluded.inbound_resp_status,
       inbound_resp_headers=excluded.inbound_resp_headers,
       inbound_resp_body=excluded.inbound_resp_body",
    params![
      r.ts,
      r.session_id,
      r.user.as_deref(),
      r.local_addr.as_deref(),
      r.mode.as_deref(),
      r.behave_as.as_deref(),
      r.peer_addr.as_deref(),
      r.method.as_deref(),
      r.request_id,
      r.request_error,
      r.endpoint,
      r.account_id,
      r.provider_id,
      r.model,
      r.initiator,
      r.status as i64,
      r.stream as i64,
      r.latency_ms.map(|v| v as i64),
      r.latency_header_ms.map(|v| v as i64),
      r.usage.input_tokens.map(|v| v as i64),
      r.usage.output_tokens.map(|v| v as i64),
      r.usage.details.cache_read.map(|v| v as i64),
      r.usage.details.reasoning.map(|v| v as i64),
      r.inbound.method.as_deref(),
      r.inbound.url.as_deref(),
      headers_json(&r.inbound.req_headers).as_ref(),
      r.inbound.req_body.as_ref(),
      opt_str(r.outbound.as_ref(), |s| s.method.as_deref()),
      opt_str(r.outbound.as_ref(), |s| s.url.as_deref()),
      r.outbound.as_ref().map(|s| headers_json(&s.req_headers)).as_ref().map(|b| b.as_ref()),
      r.outbound.as_ref().map(|s| s.req_body.as_ref()),
      r.outbound.as_ref().and_then(|s| s.status).map(|v| v as i64),
      outbound_resp_headers.as_ref().map(|b| b.as_ref()),
      r.outbound.as_ref().map(|s| s.resp_body.as_ref()),
      r.inbound.status.map(|v| v as i64),
      inbound_resp_headers.as_ref(),
      r.inbound.resp_body.as_ref(),
    ],
  )?;
  Ok(())
}

impl RequestsDb {
  #[cfg(test)]
  pub fn record(&mut self, r: &CallRecord) -> Result<()> {
    persist_record(self, r)
  }

}

impl DbEventHandler {
  pub fn on_started(
    &mut self,
    request_id: &str,
    ts: i64,
    endpoint: &str,
    session_id: Option<&str>,
    ctx: RequestContext<'_>,
    peer_addr: Option<&str>,
    method: Option<&str>,
    inbound_req: &HttpSnapshot,
  ) -> Result<()> {
    let conn = self.requests.conn_for_ts(ts)?;
    let inbound_req_headers = headers_json(&inbound_req.req_headers);
    conn.execute(
      "INSERT INTO requests (ts, session_id, user, local_addr, mode, behave_as, peer_addr, method, request_id, endpoint, account_id, provider_id, model, initiator,
                              inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, '', '', '', '', ?11, ?12, ?13, ?14)
       ON CONFLICT(request_id) DO UPDATE SET
         ts=excluded.ts,
         session_id=COALESCE(session_id, excluded.session_id),
         user=COALESCE(user, excluded.user),
         local_addr=COALESCE(local_addr, excluded.local_addr),
         mode=COALESCE(mode, excluded.mode),
         behave_as=COALESCE(behave_as, excluded.behave_as),
         peer_addr=COALESCE(peer_addr, excluded.peer_addr),
         method=COALESCE(method, excluded.method),
         endpoint=excluded.endpoint,
         inbound_req_method=excluded.inbound_req_method,
         inbound_req_url=excluded.inbound_req_url,
         inbound_req_headers=excluded.inbound_req_headers,
         inbound_req_body=excluded.inbound_req_body",
      params![
        ts,
        session_id,
        ctx.user,
        ctx.local_addr,
        ctx.mode,
        ctx.behave_as,
        peer_addr,
        method,
        request_id,
        endpoint,
        inbound_req.method.as_deref(),
        inbound_req.url.as_deref(),
        inbound_req_headers.as_ref(),
        inbound_req.req_body.as_ref(),
      ],
    )?;
    self.pending.insert(
      (request_id.to_string(), 0),
        PendingRequest {
        ts,
        session_id: session_id.map(str::to_string),
        project_id: None,
        local_addr: ctx.local_addr.map(str::to_string),
        mode: None,
          behave_as: None,
          peer_addr: peer_addr.map(str::to_string),
          method: method.map(str::to_string),
          inbound_method: inbound_req.method.clone(),
          endpoint: endpoint.to_string(),
        model: String::new(),
        initiator: String::new(),
        stream: false,
        account_id: String::new(),
        provider_id: String::new(),
        inbound_url: inbound_req.url.clone(),
        inbound_req_headers: tokn_headers::HeaderMap::new(),
        inbound_req_body: bytes::Bytes::new(),
        outbound_method: None,
        outbound_url: None,
        outbound_req_headers: tokn_headers::HeaderMap::new(),
        outbound_req_body: bytes::Bytes::new(),
        outbound_resp_headers: tokn_headers::HeaderMap::new(),
        outbound_have: false,
        latency_header_ms: None,
        result_written: false,
      },
    );
    Ok(())
  }

  pub fn on_headers(
    &mut self,
    request_id: &str,
    project_id: Option<&str>,
    header_initiator: Option<&str>,
    h: HeadersUpdate<'_>,
  ) -> Result<()> {
    let conn = self.requests.conn_for_ts(h.ts)?;
    let inbound_req_headers = headers_json(&h.inbound_req.req_headers);
    let updated = conn.execute(
      "UPDATE requests SET
         session_id=COALESCE(?2, session_id),
         local_addr=COALESCE(?3, local_addr),
         mode=COALESCE(?4, mode),
         method=COALESCE(?5, method),
         endpoint=?6,
         inbound_req_method=COALESCE(?7, inbound_req_method),
         inbound_req_url=COALESCE(?8, inbound_req_url),
         inbound_req_headers=?9
       WHERE request_id=?1",
      params![
        request_id,
        h.session_id,
        h.local_addr,
        h.mode,
        h.method,
        h.endpoint,
        h.inbound_req.method.as_deref(),
        h.inbound_req.url.as_deref(),
        inbound_req_headers.as_ref(),
      ],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %request_id, "RequestHeaders without started requests row");
    }
    let key = (request_id.to_string(), 0);
    let pending_start = self.pending.get(&key).cloned();
    let method = pending_start.as_ref().and_then(|p| p.method.as_deref());
    let url = pending_start.as_ref().and_then(|p| p.inbound_url.as_deref());
    let pending = self.pending.entry(key).or_insert_with(|| PendingRequest {
      ts: h.ts,
      session_id: h.session_id.map(str::to_string),
      project_id: project_id.map(str::to_string),
      local_addr: h.local_addr.map(str::to_string),
      mode: h.mode.map(str::to_string),
      behave_as: None,
      peer_addr: None,
      method: method.map(str::to_string),
      inbound_method: h.inbound_req.method.clone(),
      endpoint: h.endpoint.to_string(),
      model: String::new(),
      initiator: header_initiator.unwrap_or_default().to_string(),
      stream: false,
      account_id: String::new(),
      provider_id: String::new(),
      inbound_url: url.map(str::to_string),
      inbound_req_headers: h.inbound_req.req_headers.clone(),
      inbound_req_body: bytes::Bytes::new(),
      outbound_method: None,
      outbound_url: None,
      outbound_req_headers: tokn_headers::HeaderMap::new(),
      outbound_req_body: bytes::Bytes::new(),
      outbound_resp_headers: tokn_headers::HeaderMap::new(),
      outbound_have: false,
      latency_header_ms: None,
      result_written: false,
    });
    pending.ts = h.ts;
    pending.session_id = h.session_id.map(str::to_string).or_else(|| pending.session_id.clone());
    pending.project_id = project_id.map(str::to_string).or_else(|| pending.project_id.clone());
    pending.local_addr = h.local_addr.map(str::to_string).or_else(|| pending.local_addr.clone());
    pending.mode = h.mode.map(str::to_string).or_else(|| pending.mode.clone());
    pending.method = pending.method.clone().or_else(|| method.map(str::to_string));
    pending.inbound_method = pending
      .inbound_method
      .clone()
      .or_else(|| h.inbound_req.method.clone());
    if !h.endpoint.is_empty() {
      pending.endpoint = h.endpoint.to_string();
    }
    if let Some(initiator) = header_initiator {
      pending.initiator = initiator.to_string();
    }
    if pending.inbound_url.is_none() {
      pending.inbound_url = url.map(str::to_string);
    }
    pending.inbound_req_headers = h.inbound_req.req_headers.clone();
    Ok(())
  }

  pub fn on_parsed(&mut self, base_request_id: &str, attempt: u32, p: ParsedUpdate<'_>) -> Result<()> {
    let request_id = composite_request_id(base_request_id, attempt);
    let key = (base_request_id.to_string(), attempt);
    if attempt > 0 && !self.pending.contains_key(&key) {
      if let Some(base) = self.pending.get(&(base_request_id.to_string(), 0)).cloned() {
        let mut retry = base;
        retry.latency_header_ms = None;
        retry.result_written = false;
        self.pending.insert(key.clone(), retry);
      }
    }
    if let Some(pending) = self.pending.get_mut(&key) {
      pending.account_id = p.account_id.to_string();
      pending.provider_id = p.provider_id.to_string();
      pending.model = p.model.to_string();
      pending.stream = p.stream;
      pending.initiator = p.initiator.to_string();
      pending.behave_as = p.behave_as.map(str::to_string).or_else(|| pending.behave_as.clone());
      pending.inbound_req_body = p.inbound_body.clone();
    }
    let conn = self.requests.conn_for_ts(p.ts)?;
    if attempt > 0 {
      conn.execute(
      "INSERT INTO requests (ts, session_id, user, local_addr, mode, behave_as, peer_addr, method, request_id, endpoint, account_id, provider_id, model, initiator,
                                inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body)
         SELECT ts, session_id, user, local_addr, mode, behave_as, peer_addr, method, ?2, endpoint, '', '', '', '',
                 inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body
         FROM requests WHERE request_id = ?1
         ON CONFLICT(request_id) DO NOTHING",
        params![base_request_id, request_id],
      )?;
    }
    let updated = conn.execute(
      "UPDATE requests SET
         endpoint=?7,
         account_id=?2,
         provider_id=?3,
         model=?4,
         initiator=?5,
         stream=?6,
         behave_as=COALESCE(?9, behave_as),
         inbound_req_body=?8
       WHERE request_id=?1",
      params![
        request_id,
        p.account_id,
        p.provider_id,
        p.model,
        p.initiator,
        p.stream as i64,
        p.endpoint,
        p.inbound_body.as_ref(),
        p.behave_as,
      ],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %base_request_id, attempt, "RequestParsed without started requests row");
    }
    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  pub fn on_responded(
    &mut self,
    ts: i64,
    base_request_id: &str,
    attempt: u32,
    latency_header_ms: u64,
    status: u16,
    outbound_resp_headers: &tokn_headers::HeaderMap,
    outbound_req_method: Option<&str>,
    outbound_req_url: Option<&str>,
    outbound_req_headers: Option<&tokn_headers::HeaderMap>,
    outbound_req_body: Option<&bytes::Bytes>,
  ) -> Result<()> {
    let request_id = composite_request_id(base_request_id, attempt);
    let key = (base_request_id.to_string(), attempt);
    if let Some(p) = self.pending.get_mut(&key) {
      p.latency_header_ms = Some(latency_header_ms);
      if outbound_req_method.is_some() {
        p.outbound_method = outbound_req_method.map(str::to_string);
      }
      if outbound_req_url.is_some() {
        p.outbound_url = outbound_req_url.map(str::to_string);
      }
      if let Some(h) = outbound_req_headers {
        p.outbound_req_headers = h.clone();
      }
      if let Some(b) = outbound_req_body {
        p.outbound_req_body = b.clone();
      }
      p.outbound_resp_headers = outbound_resp_headers.clone();
      p.outbound_have = true;
    }
    let conn = self.requests.conn_for_ts(ts)?;
    let outbound_resp_headers_json = headers_json(outbound_resp_headers);
    let outbound_req_headers_json = outbound_req_headers.map(headers_json);
    let updated = conn.execute(
      "UPDATE requests SET
         status=?2,
         latency_header_ms=?3,
         outbound_resp_status=?2,
         outbound_resp_headers=?4,
         outbound_req_method=?5,
         outbound_req_url=?6,
         outbound_req_headers=?7,
         outbound_req_body=?8
       WHERE request_id=?1",
      params![
        request_id,
        status as i64,
        latency_header_ms as i64,
        outbound_resp_headers_json.as_ref(),
        outbound_req_method,
        outbound_req_url,
        outbound_req_headers_json.as_ref().map(|b| b.as_ref()),
        outbound_req_body.map(|b| b.as_ref()),
      ],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %base_request_id, attempt, "RequestResponded without started requests row");
    }
    Ok(())
  }

  pub fn on_result(
    &mut self,
    request_id: &str,
    attempt: u32,
    session_source: SessionSource,
    latency_ms: u64,
    inbound_status: u16,
    usage: &Usage,
    request_error: Option<&str>,
    inbound_resp_headers: &tokn_headers::HeaderMap,
    inbound_resp_body: &bytes::Bytes,
    outbound_resp_body: Option<&bytes::Bytes>,
    messages: &[crate::MessageRecord],
  ) -> Result<()> {
    let key = (request_id.to_string(), attempt);
    let composite_id = composite_request_id(request_id, attempt);
    let pending = if attempt == 0 {
      self.pending.get_mut(&key).map(|p| {
        p.result_written = true;
        p.clone()
      })
    } else {
      self.pending.remove(&key)
    };
    let record = if let Some(p) = pending {
      let outbound = if p.outbound_have || outbound_resp_body.is_some() {
        Some(HttpSnapshot {
          method: p.outbound_method.clone(),
          url: p.outbound_url.clone(),
          status: Some(inbound_status),
          req_headers: p.outbound_req_headers.clone(),
          req_body: p.outbound_req_body.clone(),
          resp_headers: p.outbound_resp_headers.clone(),
          resp_body: outbound_resp_body.cloned().unwrap_or_default(),
        })
      } else {
        None
      };
      let inbound = HttpSnapshot {
        method: p.method.clone(),
        url: p.inbound_url.clone(),
        status: Some(inbound_status),
        req_headers: p.inbound_req_headers.clone(),
        req_body: p.inbound_req_body.clone(),
        resp_headers: inbound_resp_headers.clone(),
        resp_body: inbound_resp_body.clone(),
      };
      CallRecord {
        ts: p.ts,
        session_id: p.session_id.unwrap_or_default(),
        session_source,
        user: None,
        local_addr: p.local_addr,
        mode: p.mode,
        behave_as: p.behave_as,
        peer_addr: p.peer_addr,
        method: p.method,
        request_id: composite_id,
        request_error: request_error.map(str::to_string),
        project_id: p.project_id,
        endpoint: p.endpoint,
        account_id: p.account_id,
        provider_id: p.provider_id,
        model: p.model,
        initiator: p.initiator,
        status: inbound_status,
        stream: p.stream,
        latency_ms: Some(latency_ms),
        latency_header_ms: p.latency_header_ms,
        usage: usage.clone(),
        inbound,
        outbound,
        messages: messages.to_vec(),
      }
    } else {
      let fallback_ts = Self::fallback_ts();
      tracing::warn!(request_id = %request_id, attempt, fallback_ts, "RequestResult without prior RequestParsed; persisting with current timestamp");
      CallRecord {
        ts: fallback_ts,
        session_id: String::new(),
        session_source,
        user: None,
        local_addr: None,
        mode: None,
        behave_as: None,
        peer_addr: None,
        method: None,
        request_id: composite_id,
        request_error: request_error.map(str::to_string),
        project_id: None,
        endpoint: String::new(),
        account_id: String::new(),
        provider_id: String::new(),
        model: String::new(),
        initiator: String::new(),
        status: inbound_status,
        stream: false,
        latency_ms: Some(latency_ms),
        latency_header_ms: None,
        usage: usage.clone(),
        inbound: HttpSnapshot {
          status: Some(inbound_status),
          resp_headers: inbound_resp_headers.clone(),
          resp_body: inbound_resp_body.clone(),
          ..Default::default()
        },
        outbound: outbound_resp_body.map(|b| HttpSnapshot {
          status: Some(inbound_status),
          resp_body: b.clone(),
          ..Default::default()
        }),
        messages: messages.to_vec(),
      }
    };
    persist_record(&mut self.requests, &record)?;
    write_record(&mut self.usage, &mut self.sessions, &record);
    Ok(())
  }

  pub fn on_completed(
    &mut self,
    request_id: &str,
    success: bool,
    final_status: Option<u16>,
    error: Option<&str>,
  ) -> Result<()> {
    if !success {
      let key = (request_id.to_string(), 0);
      if let Some(p) = self.pending.get(&key).cloned().filter(|p| !p.result_written) {
        let inbound = HttpSnapshot {
          method: p.method.clone(),
          url: p.inbound_url.clone(),
          status: final_status,
          req_headers: p.inbound_req_headers.clone(),
          req_body: p.inbound_req_body.clone(),
          resp_headers: tokn_headers::HeaderMap::new(),
          resp_body: bytes::Bytes::new(),
        };
        let outbound = if p.outbound_have {
          Some(HttpSnapshot {
            method: p.outbound_method.clone(),
            url: p.outbound_url.clone(),
            status: final_status,
            req_headers: p.outbound_req_headers.clone(),
            req_body: p.outbound_req_body.clone(),
            resp_headers: p.outbound_resp_headers.clone(),
            resp_body: bytes::Bytes::new(),
          })
        } else {
          None
        };
        let record = CallRecord {
          ts: p.ts,
          session_id: p.session_id.unwrap_or_default(),
          session_source: SessionSource::Header,
          user: None,
          local_addr: p.local_addr,
          mode: p.mode,
          behave_as: p.behave_as,
          peer_addr: p.peer_addr,
          method: p.method,
          request_id: request_id.to_string(),
          request_error: error.map(str::to_string),
          project_id: p.project_id,
          endpoint: p.endpoint,
          account_id: p.account_id,
          provider_id: p.provider_id,
          model: p.model,
          initiator: p.initiator,
          status: final_status.unwrap_or(0),
          stream: p.stream,
          latency_ms: None,
          latency_header_ms: p.latency_header_ms,
          usage: Usage::default(),
          inbound,
          outbound,
          messages: Vec::new(),
        };
        persist_record(&mut self.requests, &record)?;
        write_record(&mut self.usage, &mut self.sessions, &record);
      }
    }
    self.pending.retain(|(id, _), _| id != request_id);
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::super::{day_key, open_day_db};
  use super::*;
  use crate::{CallRecord, DbPaths, HttpSnapshot, SessionSource, Usage};
  use rusqlite::Connection;

  fn make_handler(root: std::path::PathBuf) -> DbEventHandler {
    DbEventHandler::new(DbPaths {
      usage_db: root.join("usage.db"),
      sessions_db: root.join("sessions.db"),
      requests_dir: root.join("requests"),
    })
    .unwrap()
  }

  #[test]
  fn migrates_v1_day_file_and_records_request_error() {
    let dir = std::env::temp_dir().join(format!("tokn-router-req-mig-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let ts = 100;
    let path = dir.join(format!("{}.db", day_key(ts)));

    {
      let conn = Connection::open(&path).unwrap();
      conn
        .execute_batch(
          "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_ts INTEGER NOT NULL);",
        )
        .unwrap();
      conn
        .execute_batch(include_str!("../../migrations/requests/001_initial.sql"))
        .unwrap();
      conn
        .execute(
          "INSERT INTO schema_migrations (version, name, applied_ts) VALUES (1, 'initial', 0)",
          [],
        )
        .unwrap();
    }

    let mut db = RequestsDb::new(dir).unwrap();
    db.record(&CallRecord {
      ts,
      session_id: "session-1".into(),
      session_source: SessionSource::Header,
      user: None,
      local_addr: None,
      mode: None,
      behave_as: None,
      peer_addr: Some("127.0.0.1:4142".into()),
      method: Some("POST".into()),
      request_id: "request-1".into(),
      request_error: Some("stream terminated before completion".into()),
      project_id: Some("project-1".into()),
      endpoint: "chat_completions".into(),
      account_id: "account".into(),
      provider_id: "provider".into(),
      model: "model".into(),
      initiator: "user".into(),
      status: 200,
      stream: true,
      latency_ms: Some(1),
      latency_header_ms: Some(1),
      usage: Usage::default(),
      inbound: HttpSnapshot::default(),
      outbound: None,
      messages: Vec::new(),
    })
    .unwrap();

    let conn = open_day_db(&path).unwrap();
    let row: (String, String) = conn
      .query_row("SELECT request_id, request_error FROM requests", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
      })
      .unwrap();
    assert_eq!(row, ("request-1".into(), "stream terminated before completion".into()));
  }

  #[test]
  fn headers_updates_existing_started_row() {
    let root = std::env::temp_dir().join(format!("tokn-router-req-headers-{}", uuid::Uuid::new_v4()));
    let dir = root.join("requests");
    std::fs::create_dir_all(&dir).unwrap();
    let mut handler = make_handler(root);
    let ts = 1_700_000_000;
    let request_id = "request-headers";
    let started = HttpSnapshot {
      method: Some("POST".into()),
      url: Some("https://example.test/original".into()),
      ..Default::default()
    };
    handler.on_started(
      request_id,
      ts,
      "chat_completions",
      Some("sess-1"),
      RequestContext::default(),
      Some("127.0.0.1:4142"),
      Some("requests"),
      &started,
    )
    .unwrap();

    let mut headers = tokn_headers::HeaderMap::new();
    headers.insert("x-test", "1");
    let with_headers = HttpSnapshot {
      method: Some("POST".into()),
      url: Some("https://example.test/original".into()),
      req_headers: headers,
      ..Default::default()
    };
    handler.on_headers(
      request_id,
      None,
      None,
      HeadersUpdate {
        ts,
        endpoint: "responses",
        session_id: Some("sess-1"),
        local_addr: Some("localhost:4141"),
        mode: Some("route"),
        method: Some("requests"),
        inbound_req: &with_headers,
      },
    )
    .unwrap();

    let conn = open_day_db(&dir.join(format!("{}.db", day_key(ts)))).unwrap();
    let row: (i64, String, Option<String>, Option<String>, Vec<u8>) = conn
      .query_row(
        "SELECT COUNT(*), endpoint, peer_addr, method, inbound_req_headers FROM requests WHERE request_id = ?1",
        params![request_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
      )
      .unwrap();
    assert_eq!(row.0, 1);
    assert_eq!(row.1, "responses");
    assert_eq!(row.2.as_deref(), Some("127.0.0.1:4142"));
    assert_eq!(row.3.as_deref(), Some("requests"));
    assert!(String::from_utf8(row.4).unwrap().contains("\"x-test\":\"1\""));
  }

  #[test]
  fn result_insert_can_backfill_source_and_method() {
    let root = std::env::temp_dir().join(format!("tokn-router-req-result-{}", uuid::Uuid::new_v4()));
    let dir = root.join("requests");
    std::fs::create_dir_all(&dir).unwrap();
    let mut handler = make_handler(root);
    let ts = 1_700_000_000;

    let record = CallRecord {
      ts,
      session_id: "session-1".into(),
      session_source: SessionSource::Header,
      user: None,
      local_addr: None,
      mode: None,
      behave_as: None,
      peer_addr: Some("127.0.0.1:4142".into()),
      method: Some("POST".into()),
      request_id: "request-result".into(),
      request_error: None,
      project_id: Some("project-1".into()),
      endpoint: "responses".into(),
      account_id: "account".into(),
      provider_id: "provider".into(),
      model: "model".into(),
      initiator: "user".into(),
      status: 200,
      stream: false,
      latency_ms: Some(10),
      latency_header_ms: Some(4),
      usage: Usage::default(),
      inbound: HttpSnapshot::default(),
      outbound: None,
      messages: Vec::new(),
    };
    persist_record(&mut handler.requests, &record).unwrap();

    let conn = open_day_db(&dir.join(format!("{}.db", day_key(ts)))).unwrap();
    let row: (Option<String>, Option<String>) = conn
      .query_row(
        "SELECT peer_addr, method FROM requests WHERE request_id = 'request-result'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
      )
      .unwrap();
    assert_eq!(row.0.as_deref(), Some("127.0.0.1:4142"));
    assert_eq!(row.1.as_deref(), Some("POST"));
  }
}

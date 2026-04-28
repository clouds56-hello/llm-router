use super::{CallRecord, Result};
use rusqlite::{params, Connection};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use time::macros::format_description;

const CACHE_CAP: usize = 3;

pub struct RequestsDb {
  dir: PathBuf,
  conns: HashMap<String, Connection>,
  order: VecDeque<String>,
}

impl RequestsDb {
  pub fn new(dir: PathBuf) -> Result<Self> {
    std::fs::create_dir_all(&dir)?;
    Ok(Self {
      dir,
      conns: HashMap::new(),
      order: VecDeque::new(),
    })
  }

  pub fn record(&mut self, r: &CallRecord) -> Result<()> {
    let conn = self.conn_for_ts(r.ts)?;
    conn.execute(
      "INSERT INTO requests (ts, session_id, endpoint, account_id, provider_id, model, initiator, status, stream, latency_ms,
                             prompt_tok, completion_tok, req_headers, req_body, resp_headers, resp_body)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
      params![
        r.ts,
        r.session_id,
        r.endpoint,
        r.account_id,
        r.provider_id,
        r.model,
        r.initiator,
        r.status as i64,
        r.stream as i64,
        r.latency_ms as i64,
        r.prompt_tokens.map(|v| v as i64),
        r.completion_tokens.map(|v| v as i64),
        r.req_headers.as_ref(),
        r.req_body.as_ref(),
        r.resp_headers.as_ref().map(|b| b.as_ref()),
        r.resp_body.as_ref().map(|b| b.as_ref()),
      ],
    )?;
    Ok(())
  }

  fn conn_for_ts(&mut self, ts: i64) -> Result<&mut Connection> {
    let key = day_key(ts);
    if !self.conns.contains_key(&key) {
      if self.order.len() >= CACHE_CAP {
        if let Some(old) = self.order.pop_front() {
          self.conns.remove(&old);
        }
      }
      let conn = open_day_db(&self.dir.join(format!("{key}.db")))?;
      self.conns.insert(key.clone(), conn);
    }
    self.order.retain(|k| k != &key);
    self.order.push_back(key.clone());
    Ok(self.conns.get_mut(&key).expect("opened requests db"))
  }
}

fn open_day_db(path: &Path) -> Result<Connection> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
  }
  let conn = Connection::open(path)?;
  conn.execute_batch(
    r#"
    CREATE TABLE IF NOT EXISTS requests (
      id INTEGER PRIMARY KEY,
      ts INTEGER NOT NULL,
      session_id TEXT,
      endpoint TEXT NOT NULL,
      account_id TEXT NOT NULL,
      provider_id TEXT NOT NULL,
      model TEXT NOT NULL,
      initiator TEXT NOT NULL,
      status INTEGER NOT NULL,
      stream INTEGER NOT NULL,
      latency_ms INTEGER NOT NULL,
      prompt_tok INTEGER,
      completion_tok INTEGER,
      req_headers BLOB NOT NULL,
      req_body BLOB NOT NULL,
      resp_headers BLOB,
      resp_body BLOB
    );
    CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
    CREATE INDEX IF NOT EXISTS idx_requests_session ON requests(session_id);
    CREATE INDEX IF NOT EXISTS idx_requests_account ON requests(account_id);
    "#,
  )?;
  Ok(conn)
}

fn day_key(ts: i64) -> String {
  let dt = time::OffsetDateTime::from_unix_timestamp(ts).unwrap_or_else(|_| time::OffsetDateTime::now_utc());
  dt.date()
    .format(format_description!("[year]-[month]-[day]"))
    .unwrap_or_else(|_| "1970-01-01".to_string())
}

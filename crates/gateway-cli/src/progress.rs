//! TTY progress display for in-flight requests.
//!
//! Implements [`EventHandler`] driving an [`indicatif::MultiProgress`].
//! One bar per request_id; updated on lifecycle events; finalised on
//! `RequestCompleted` and persisted in the scrollback. A persistent
//! footer bar (last line of the MultiProgress) shows live session
//! counters.
//!
//! Only registered when stdout is a TTY (see `server_runtime.rs`).

use console::style;
use crate::db::archive::{ArchiveEvent, ArchiveEventHandler};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use llm_core::db::Usage;
use llm_core::event::{Event, EventHandler};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;
use time::{macros::format_description, OffsetDateTime};

/// Process-wide [`MultiProgress`] shared between [`ProgressEventHandler`]
/// and the tracing log writer (so log lines suspend the bars during
/// emission instead of garbling them).
static MULTI: OnceLock<MultiProgress> = OnceLock::new();

/// Returns the shared [`MultiProgress`]. Lazily initialized on first call;
/// safe to call from any context (logging init or event handler).
pub fn multi() -> &'static MultiProgress {
  MULTI.get_or_init(|| MultiProgress::with_draw_target(ProgressDrawTarget::stdout()))
}

struct RequestState {
  started: Instant,
  provider: String,
  model: String,
  account: String,
  endpoint: String,
  attempt: u32,
  sent_bytes: u64,
  recv_bytes: u64,
  usage: Usage,
}

impl RequestState {
  fn new(endpoint: String) -> Self {
    Self {
      started: Instant::now(),
      provider: String::new(),
      model: String::new(),
      account: String::new(),
      endpoint,
      attempt: 0,
      sent_bytes: 0,
      recv_bytes: 0,
      usage: Usage::default(),
    }
  }

  fn id_short(request_id: &str) -> String {
    request_id.chars().take(8).collect()
  }

  fn render_in_flight(&self, request_id: &str) -> String {
    let elapsed = self.started.elapsed().as_secs_f64();
    let speed_kbs = if elapsed > 0.05 {
      (self.recv_bytes as f64) / 1024.0 / elapsed
    } else {
      0.0
    };
    let attempt_part = if self.attempt > 0 {
      format!(" {}", style(format!("a={}", self.attempt)).yellow())
    } else {
      String::new()
    };
    format!(
      "[{}] {} {} {}{} {} sent={:.1}kB recv={:.1}kB {:.1}kB/s elapsed={:.1}s",
      style(Self::id_short(request_id)).dim(),
      style(&self.provider).blue(),
      style(truncate(&self.model, 28)).cyan(),
      style(truncate(&self.account, 16)).magenta(),
      attempt_part,
      style(&self.endpoint).dim(),
      (self.sent_bytes as f64) / 1024.0,
      (self.recv_bytes as f64) / 1024.0,
      speed_kbs,
      elapsed,
    )
  }

  fn merge_usage(&mut self, usage: &Usage) {
    if usage.input_tokens.is_some() {
      self.usage.input_tokens = usage.input_tokens;
    }
    if usage.output_tokens.is_some() {
      self.usage.output_tokens = usage.output_tokens;
    }
    if usage.details.cache_read.is_some() {
      self.usage.details.cache_read = usage.details.cache_read;
    }
    if usage.details.reasoning.is_some() {
      self.usage.details.reasoning = usage.details.reasoning;
    }
  }

  fn render_completed(
    &self,
    request_id: &str,
    success: bool,
    total_attempts: u32,
    final_status: Option<u16>,
    total_latency_ms: u64,
    error: Option<&str>,
  ) -> String {
    let id_short = Self::id_short(request_id);
    let latency_s = (total_latency_ms as f64) / 1000.0;
    let attempts_part = if total_attempts > 1 {
      format!(" attempts={total_attempts}")
    } else {
      String::new()
    };
    if success {
      let status = final_status.unwrap_or(0);
      format!(
        "[{}] {} {} {} {} {} {} sent={:.1}kB recv={:.1}kB{} latency={:.1}s{}",
        style(&id_short).dim(),
        style("✓").green().bold(),
        style_status(status),
        style(&self.provider).blue(),
        style(truncate(&self.model, 28)).cyan(),
        style(truncate(&self.account, 16)).magenta(),
        style(&self.endpoint).dim(),
        (self.sent_bytes as f64) / 1024.0,
        (self.recv_bytes as f64) / 1024.0,
        format_usage(&self.usage),
        latency_s,
        attempts_part,
      )
    } else {
      let err = error.unwrap_or("failed");
      let status_part = match final_status {
        Some(s) => format!(" {}", style_status(s)),
        None => String::new(),
      };
      format!(
        "[{}] {}{} {} {} {} {} sent={:.1}kB recv={:.1}kB latency={:.1}s{} error={}",
        style(&id_short).dim(),
        style("✗").red().bold(),
        status_part,
        style(&self.provider).blue(),
        style(truncate(&self.model, 28)).cyan(),
        style(truncate(&self.account, 16)).magenta(),
        style(&self.endpoint).dim(),
        (self.sent_bytes as f64) / 1024.0,
        (self.recv_bytes as f64) / 1024.0,
        latency_s,
        attempts_part,
        style(truncate(err, 80)).red(),
      )
    }
  }

  fn render_interrupted(&self, request_id: &str) -> String {
    let id_short = Self::id_short(request_id);
    let elapsed = self.started.elapsed().as_secs_f64();
    let model_part = if self.model.is_empty() {
      String::new()
    } else {
      format!(" {}", style(truncate(&self.model, 28)).cyan())
    };
    let account_part = if self.account.is_empty() {
      String::new()
    } else {
      format!(" {}", style(truncate(&self.account, 16)).magenta())
    };
    format!(
      "[{}] {}{}{} sent={:.1}kB recv={:.1}kB elapsed={:.1}s",
      style(&id_short).dim(),
      style("⚠ interrupted").yellow().bold(),
      model_part,
      account_part,
      (self.sent_bytes as f64) / 1024.0,
      (self.recv_bytes as f64) / 1024.0,
      elapsed,
    )
  }
}

struct BarState {
  bar: ProgressBar,
  request: RequestState,
}

fn endpoint_label(endpoint: &str, url: Option<&str>) -> String {
  let endpoint = endpoint.trim();
  if endpoint.is_empty() || endpoint.eq_ignore_ascii_case("unknown") {
    url
      .map(str::trim)
      .filter(|value| !value.is_empty())
      .unwrap_or("unknown")
      .to_string()
  } else {
    endpoint.to_string()
  }
}

fn truncate(s: &str, max: usize) -> &str {
  if s.len() <= max {
    s
  } else {
    &s[..max]
  }
}

fn file_label(path: &Path) -> String {
  path
    .file_name()
    .and_then(|v| v.to_str())
    .unwrap_or_else(|| path.to_str().unwrap_or("unknown"))
    .to_string()
}

fn style_status(status: u16) -> console::StyledObject<u16> {
  match status {
    200..=299 => style(status).green(),
    300..=399 => style(status).cyan(),
    400..=499 => style(status).yellow(),
    500..=599 => style(status).red(),
    _ => style(status),
  }
}

/// Format a [`Usage`] for the success line: ` in=N out=M cache=K reason=R`,
/// omitting any field whose value is None or 0. Returns an empty string if no
/// fields are set; otherwise the result starts with a leading space.
fn format_usage(u: &Usage) -> String {
  let mut parts = Vec::with_capacity(4);
  if let Some(v) = u.input_tokens {
    if v > 0 {
      parts.push(format!("in={v}"));
    }
  }
  if let Some(v) = u.output_tokens {
    if v > 0 {
      parts.push(format!("out={v}"));
    }
  }
  if let Some(v) = u.details.cache_read {
    if v > 0 {
      parts.push(format!("cache={v}"));
    }
  }
  if let Some(v) = u.details.reasoning {
    if v > 0 {
      parts.push(format!("reason={v}"));
    }
  }
  if parts.is_empty() {
    String::new()
  } else {
    format!(" {}", parts.join(" "))
  }
}

pub struct ProgressEventHandler {
  multi: MultiProgress,
  bars: HashMap<String, BarState>,
  /// Style for in-flight bars (spinner + dynamic message).
  style: ProgressStyle,
  /// Persistent footer bar (last line) showing session counters.
  footer: ProgressBar,
  /// Session counters.
  in_flight: u64,
  completed: u64,
  errors: u64,
}

impl ProgressEventHandler {
  pub fn new() -> Self {
    let multi = multi().clone();
    let style = ProgressStyle::with_template("{spinner:.cyan} {msg}")
      .unwrap_or_else(|_| ProgressStyle::default_spinner())
      .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

    // Footer bar: a borderless line that never finishes, always at the bottom.
    let footer = multi.add(ProgressBar::new_spinner());
    let footer_style = ProgressStyle::with_template("{msg}").unwrap_or_else(|_| ProgressStyle::default_spinner());
    footer.set_style(footer_style);

    let handler = Self {
      multi,
      bars: HashMap::new(),
      style,
      footer,
      in_flight: 0,
      completed: 0,
      errors: 0,
    };
    handler.refresh_footer();
    handler
  }

  fn refresh(&mut self, request_id: &str) {
    if let Some(state) = self.bars.get(request_id) {
      let msg = state.request.render_in_flight(request_id);
      state.bar.set_message(msg);
      state.bar.tick();
    }
  }

  fn refresh_footer(&self) {
    let errors_part = if self.errors > 0 {
      format!("errors={}", style(self.errors).red())
    } else {
      format!("errors={}", self.errors)
    };
    let msg = format!(
      "─── in-flight={} completed={} {} ───",
      style(self.in_flight).bold(),
      style(self.completed).green(),
      errors_part,
    );
    self.footer.set_message(msg);
    self.footer.tick();
  }
}

impl Default for ProgressEventHandler {
  fn default() -> Self {
    Self::new()
  }
}

impl EventHandler for ProgressEventHandler {
  fn handle(&mut self, event: &Event) {
    match event {
      Event::RequestStarted {
        request_id,
        endpoint,
        ..
      } => {
        // Insert above the footer.
        let bar = self.multi.insert_before(&self.footer, ProgressBar::new_spinner());
        bar.set_style(self.style.clone());
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        let state = BarState {
          bar,
          request: RequestState::new(endpoint_label(endpoint, None)),
        };
        self.bars.insert(request_id.clone(), state);
        self.in_flight = self.in_flight.saturating_add(1);
        self.refresh(request_id);
        self.refresh_footer();
      }
      Event::RequestParsed {
        request_id,
        attempt,
        account_id,
        provider_id,
        model,
        outbound_req,
        ..
      } => {
        if let Some(state) = self.bars.get_mut(request_id) {
          state.request.provider = provider_id.clone();
          state.request.model = model.clone();
          state.request.account = account_id.clone();
          state.request.attempt = *attempt;
          if let Some(snap) = outbound_req {
            if state.request.endpoint.eq_ignore_ascii_case("unknown") || state.request.endpoint.is_empty() {
              state.request.endpoint = endpoint_label(&state.request.endpoint, snap.url.as_deref());
            }
            state.request.sent_bytes = snap.body.len() as u64;
          }
        }
        self.refresh(request_id);
      }
      Event::RequestRetry {
        request_id, attempt, ..
      } => {
        if let Some(state) = self.bars.get_mut(request_id) {
          // attempt N just failed; next try will be attempt+1.
          state.request.attempt = attempt + 1;
          state.request.recv_bytes = 0;
        }
        self.refresh(request_id);
      }
      Event::StreamProgress {
        request_id,
        bytes_streamed,
        usage,
        ..
      } => {
        if let Some(state) = self.bars.get_mut(request_id) {
          state.request.recv_bytes = *bytes_streamed;
          // Merge any non-None usage fields seen so far.
          state.request.merge_usage(usage);
        }
        self.refresh(request_id);
      }
      Event::RequestResult {
        request_id,
        inbound_resp,
        usage,
        ..
      } => {
        if let Some(state) = self.bars.get_mut(request_id) {
          let body_len = inbound_resp.body.len() as u64;
          if body_len > state.request.recv_bytes {
            state.request.recv_bytes = body_len;
          }
          state.request.usage = usage.clone();
        }
      }
      Event::RequestCompleted {
        request_id,
        success,
        total_attempts,
        final_status,
        total_latency_ms,
        error,
      } => {
        if let Some(state) = self.bars.remove(request_id) {
          let final_msg = state.request.render_completed(
            request_id,
            *success,
            *total_attempts,
            *final_status,
            *total_latency_ms,
            error.as_deref(),
          );
          state.bar.disable_steady_tick();
          let _ = self.multi.println(final_msg);
          state.bar.finish_and_clear();
        }
        // Update counters.
        self.in_flight = self.in_flight.saturating_sub(1);
        self.completed = self.completed.saturating_add(1);
        if !success {
          self.errors = self.errors.saturating_add(1);
        }
        self.refresh_footer();
      }
      _ => {}
    }
  }

  fn flush(&mut self) {
    // For each in-flight straggler: emit a one-line interrupted summary
    // via multi.println (suspends bars during emit) then clear the bar
    // so the live region shrinks. The println line lands in scrollback.
    let stragglers: Vec<(String, BarState)> = self.bars.drain().collect();
    for (request_id, state) in stragglers {
      let line = state.request.render_interrupted(&request_id);
      let _ = self.multi.println(line);
      state.bar.disable_steady_tick();
      state.bar.finish_and_clear();
    }

    // Footer: print final session summary, then clear the live footer bar.
    let interrupted_part = if self.in_flight > 0 {
      format!(" interrupted={}", style(self.in_flight).yellow())
    } else {
      String::new()
    };
    let errors_part = if self.errors > 0 {
      format!("errors={}", style(self.errors).red())
    } else {
      format!("errors={}", self.errors)
    };
    let summary = format!(
      "─── session ended: completed={} {}{} ───",
      style(self.completed).green(),
      errors_part,
      interrupted_part,
    );
    let _ = self.multi.println(summary);
    self.footer.finish_and_clear();
  }
}

pub struct ProgressLogEventHandler {
  writer: BufWriter<File>,
  requests: HashMap<String, RequestState>,
  in_flight: u64,
  completed: u64,
  errors: u64,
  write_failed: bool,
}

struct ArchiveBarState {
  bar: ProgressBar,
  started: Instant,
  path: PathBuf,
  archive: PathBuf,
  total_bytes: u64,
}

pub struct ArchiveProgressEventHandler {
  multi: MultiProgress,
  bars: HashMap<String, ArchiveBarState>,
  style: ProgressStyle,
}

impl ArchiveProgressEventHandler {
  pub fn new() -> Self {
    let multi = multi().clone();
    let style = ProgressStyle::with_template("{spinner:.yellow} {msg}")
      .unwrap_or_else(|_| ProgressStyle::default_spinner())
      .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");
    Self {
      multi,
      bars: HashMap::new(),
      style,
    }
  }

  fn refresh(&self, id: &str, bytes_read: u64, total_bytes: u64) {
    if let Some(state) = self.bars.get(id) {
      let percent = if total_bytes > 0 {
        (bytes_read as f64 * 100.0) / total_bytes as f64
      } else {
        100.0
      };
      let elapsed = state.started.elapsed().as_secs_f64();
      let speed_kbs = if elapsed > 0.05 {
        (bytes_read as f64) / 1024.0 / elapsed
      } else {
        0.0
      };
      state.bar.set_message(format!(
        "archive {} {:.1}% {:.1}/{:.1}MB {:.1}kB/s -> {}",
        style(file_label(&state.path)).yellow(),
        percent.min(100.0),
        bytes_read as f64 / 1024.0 / 1024.0,
        state.total_bytes as f64 / 1024.0 / 1024.0,
        speed_kbs,
        style(file_label(&state.archive)).dim(),
      ));
      state.bar.tick();
    }
  }
}

impl Default for ArchiveProgressEventHandler {
  fn default() -> Self {
    Self::new()
  }
}

impl ArchiveEventHandler for ArchiveProgressEventHandler {
  fn handle(&mut self, event: &ArchiveEvent) {
    match event {
      ArchiveEvent::ScanStarted { dir } => {
        tracing::debug!(path = %dir.display(), "request db archival progress scan started");
      }
      ArchiveEvent::FileStarted {
        id,
        path,
        archive,
        total_bytes,
      } => {
        let bar = self.multi.add(ProgressBar::new_spinner());
        bar.set_style(self.style.clone());
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        self.bars.insert(
          id.clone(),
          ArchiveBarState {
            bar,
            started: Instant::now(),
            path: path.clone(),
            archive: archive.clone(),
            total_bytes: *total_bytes,
          },
        );
        self.refresh(id, 0, *total_bytes);
      }
      ArchiveEvent::FileProgress {
        id,
        bytes_read,
        total_bytes,
      } => self.refresh(id, *bytes_read, *total_bytes),
      ArchiveEvent::FileCompleted {
        id,
        path,
        archive,
        bytes_in,
        bytes_out,
      } => {
        if let Some(state) = self.bars.remove(id) {
          state.bar.disable_steady_tick();
          state.bar.finish_and_clear();
        }
        let ratio = if *bytes_in > 0 {
          (*bytes_out as f64 * 100.0) / *bytes_in as f64
        } else {
          0.0
        };
        let _ = self.multi.println(format!(
          "{} archived {} -> {} {:.1}MB to {:.1}MB ({:.1}%)",
          style("✓").green().bold(),
          style(file_label(path)).yellow(),
          style(file_label(archive)).dim(),
          *bytes_in as f64 / 1024.0 / 1024.0,
          *bytes_out as f64 / 1024.0 / 1024.0,
          ratio,
        ));
      }
      ArchiveEvent::FileSkipped { path, archive } => {
        tracing::debug!(path = %path.display(), archive = %archive.display(), "request db archive already exists");
      }
      ArchiveEvent::FileFailed { id, path, archive, error } => {
        if let Some(state) = self.bars.remove(id) {
          state.bar.disable_steady_tick();
          state.bar.finish_and_clear();
        }
        let _ = self.multi.println(format!(
          "{} archive {} -> {} failed: {}",
          style("✗").red().bold(),
          style(file_label(path)).yellow(),
          style(file_label(archive)).dim(),
          style(truncate(error, 120)).red(),
        ));
      }
      ArchiveEvent::ScanCompleted { dir, stats } => {
        tracing::debug!(path = %dir.display(), archived = stats.archived, skipped_existing = stats.skipped_existing, failed = stats.failed, "request db archival progress scan completed");
      }
    }
  }

  fn flush(&mut self) {
    let bars: Vec<ArchiveBarState> = self.bars.drain().map(|(_, state)| state).collect();
    for state in bars {
      let _ = self.multi.println(format!(
        "{} archive {} interrupted",
        style("⚠").yellow().bold(),
        style(file_label(&state.path)).yellow(),
      ));
      state.bar.disable_steady_tick();
      state.bar.finish_and_clear();
    }
  }
}

impl ProgressLogEventHandler {
  pub fn new(log_dir: &Path) -> io::Result<Self> {
    std::fs::create_dir_all(log_dir)?;
    let file = OpenOptions::new()
      .create(true)
      .append(true)
      .open(progress_log_path(log_dir))?;
    Ok(Self {
      writer: BufWriter::new(file),
      requests: HashMap::new(),
      in_flight: 0,
      completed: 0,
      errors: 0,
      write_failed: false,
    })
  }

  fn write_line(&mut self, line: &str) {
    if self.write_failed {
      return;
    }
    if let Err(e) = writeln!(self.writer, "{line}").and_then(|_| self.writer.flush()) {
      self.write_failed = true;
      tracing::warn!(error = %e, "failed to write progress log");
    }
  }

  fn summary(&self) -> String {
    let interrupted_part = if self.in_flight > 0 {
      format!(" interrupted={}", style(self.in_flight).yellow())
    } else {
      String::new()
    };
    let errors_part = if self.errors > 0 {
      format!("errors={}", style(self.errors).red())
    } else {
      format!("errors={}", self.errors)
    };
    format!(
      "─── session ended: completed={} {}{} ───",
      style(self.completed).green(),
      errors_part,
      interrupted_part,
    )
  }
}

impl EventHandler for ProgressLogEventHandler {
  fn handle(&mut self, event: &Event) {
    match event {
      Event::RequestStarted {
        request_id,
        endpoint,
        ..
      } => {
        self.requests.insert(
          request_id.clone(),
          RequestState::new(endpoint_label(endpoint, None)),
        );
        self.in_flight = self.in_flight.saturating_add(1);
      }
      Event::RequestParsed {
        request_id,
        attempt,
        account_id,
        provider_id,
        model,
        outbound_req,
        ..
      } => {
        if let Some(state) = self.requests.get_mut(request_id) {
          state.provider = provider_id.clone();
          state.model = model.clone();
          state.account = account_id.clone();
          state.attempt = *attempt;
          if let Some(snap) = outbound_req {
            if state.endpoint.eq_ignore_ascii_case("unknown") || state.endpoint.is_empty() {
              state.endpoint = endpoint_label(&state.endpoint, snap.url.as_deref());
            }
            state.sent_bytes = snap.body.len() as u64;
          }
        }
      }
      Event::RequestRetry {
        request_id, attempt, ..
      } => {
        if let Some(state) = self.requests.get_mut(request_id) {
          state.attempt = attempt + 1;
          state.recv_bytes = 0;
        }
      }
      Event::StreamProgress {
        request_id,
        bytes_streamed,
        usage,
        ..
      } => {
        if let Some(state) = self.requests.get_mut(request_id) {
          state.recv_bytes = *bytes_streamed;
          state.merge_usage(usage);
        }
      }
      Event::RequestResult {
        request_id,
        inbound_resp,
        usage,
        ..
      } => {
        if let Some(state) = self.requests.get_mut(request_id) {
          let body_len = inbound_resp.body.len() as u64;
          if body_len > state.recv_bytes {
            state.recv_bytes = body_len;
          }
          state.usage = usage.clone();
        }
      }
      Event::RequestCompleted {
        request_id,
        success,
        total_attempts,
        final_status,
        total_latency_ms,
        error,
      } => {
        if let Some(state) = self.requests.remove(request_id) {
          let line = state.render_completed(
            request_id,
            *success,
            *total_attempts,
            *final_status,
            *total_latency_ms,
            error.as_deref(),
          );
          self.write_line(&line);
        }
        self.in_flight = self.in_flight.saturating_sub(1);
        self.completed = self.completed.saturating_add(1);
        if !success {
          self.errors = self.errors.saturating_add(1);
        }
      }
      _ => {}
    }
  }

  fn flush(&mut self) {
    let stragglers: Vec<(String, RequestState)> = self.requests.drain().collect();
    for (request_id, state) in stragglers {
      let line = state.render_interrupted(&request_id);
      self.write_line(&line);
    }
    let summary = self.summary();
    self.write_line(&summary);
  }
}

fn progress_log_path(log_dir: &Path) -> PathBuf {
  let date = OffsetDateTime::now_utc()
    .format(format_description!("[year]-[month]-[day]"))
    .unwrap_or_else(|_| "unknown-date".to_string());
  log_dir.join(format!("llm-router-progress.log.{date}"))
}

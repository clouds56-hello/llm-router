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
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use llm_core::db::Usage;
use llm_core::event::{Event, EventHandler};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

/// Process-wide [`MultiProgress`] shared between [`ProgressEventHandler`]
/// and the tracing log writer (so log lines suspend the bars during
/// emission instead of garbling them).
static MULTI: OnceLock<MultiProgress> = OnceLock::new();

/// Returns the shared [`MultiProgress`]. Lazily initialized on first call;
/// safe to call from any context (logging init or event handler).
pub fn multi() -> &'static MultiProgress {
  MULTI.get_or_init(|| MultiProgress::with_draw_target(ProgressDrawTarget::stdout()))
}

struct BarState {
  bar: ProgressBar,
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

impl BarState {
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
}

fn truncate(s: &str, max: usize) -> &str {
  if s.len() <= max {
    s
  } else {
    &s[..max]
  }
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
    if v > 0 { parts.push(format!("in={v}")); }
  }
  if let Some(v) = u.output_tokens {
    if v > 0 { parts.push(format!("out={v}")); }
  }
  if let Some(v) = u.details.cache_read {
    if v > 0 { parts.push(format!("cache={v}")); }
  }
  if let Some(v) = u.details.reasoning {
    if v > 0 { parts.push(format!("reason={v}")); }
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
    let footer_style = ProgressStyle::with_template("{msg}")
      .unwrap_or_else(|_| ProgressStyle::default_spinner());
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
      let msg = state.render_in_flight(request_id);
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
        request_id, endpoint, ..
      } => {
        // Insert above the footer.
        let bar = self
          .multi
          .insert_before(&self.footer, ProgressBar::new_spinner());
        bar.set_style(self.style.clone());
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        let state = BarState {
          bar,
          started: Instant::now(),
          provider: String::new(),
          model: String::new(),
          account: String::new(),
          endpoint: endpoint.clone(),
          attempt: 0,
          sent_bytes: 0,
          recv_bytes: 0,
          usage: Usage::default(),
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
          state.provider = provider_id.clone();
          state.model = model.clone();
          state.account = account_id.clone();
          state.attempt = *attempt;
          if let Some(snap) = outbound_req {
            state.sent_bytes = snap.body.len() as u64;
          }
        }
        self.refresh(request_id);
      }
      Event::RequestRetry {
        request_id, attempt, ..
      } => {
        if let Some(state) = self.bars.get_mut(request_id) {
          // attempt N just failed; next try will be attempt+1.
          state.attempt = attempt + 1;
          state.recv_bytes = 0;
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
          state.recv_bytes = *bytes_streamed;
          // Merge any non-None usage fields seen so far.
          if usage.input_tokens.is_some() { state.usage.input_tokens = usage.input_tokens; }
          if usage.output_tokens.is_some() { state.usage.output_tokens = usage.output_tokens; }
          if usage.details.cache_read.is_some() { state.usage.details.cache_read = usage.details.cache_read; }
          if usage.details.reasoning.is_some() { state.usage.details.reasoning = usage.details.reasoning; }
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
        if let Some(state) = self.bars.remove(request_id) {
          let id_short = BarState::id_short(request_id);
          let latency_s = (*total_latency_ms as f64) / 1000.0;
          let attempts_part = if *total_attempts > 1 {
            format!(" attempts={}", total_attempts)
          } else {
            String::new()
          };
          let final_msg = if *success {
            let status = final_status.unwrap_or(0);
            format!(
              "[{}] {} {} {} {} sent={:.1}kB recv={:.1}kB{} latency={:.1}s{}",
              style(&id_short).dim(),
              style("✓").green().bold(),
              style_status(status),
              style(truncate(&state.model, 28)).cyan(),
              style(truncate(&state.account, 16)).magenta(),
              (state.sent_bytes as f64) / 1024.0,
              (state.recv_bytes as f64) / 1024.0,
              format_usage(&state.usage),
              latency_s,
              attempts_part,
            )
          } else {
            let err = error.as_deref().unwrap_or("failed");
            let status_part = match final_status {
              Some(s) => format!(" {}", style_status(*s)),
              None => String::new(),
            };
            format!(
              "[{}] {}{} {} {} sent={:.1}kB recv={:.1}kB latency={:.1}s{} error={}",
              style(&id_short).dim(),
              style("✗").red().bold(),
              status_part,
              style(truncate(&state.model, 28)).cyan(),
              style(truncate(&state.account, 16)).magenta(),
              (state.sent_bytes as f64) / 1024.0,
              (state.recv_bytes as f64) / 1024.0,
              latency_s,
              attempts_part,
              style(truncate(err, 80)).red(),
            )
          };
          state.bar.disable_steady_tick();
          state.bar.finish_with_message(final_msg);
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
      let id_short = BarState::id_short(&request_id);
      let elapsed = state.started.elapsed().as_secs_f64();
      let model_part = if state.model.is_empty() {
        String::new()
      } else {
        format!(" {}", style(truncate(&state.model, 28)).cyan())
      };
      let account_part = if state.account.is_empty() {
        String::new()
      } else {
        format!(" {}", style(truncate(&state.account, 16)).magenta())
      };
      let line = format!(
        "[{}] {}{}{} sent={:.1}kB recv={:.1}kB elapsed={:.1}s",
        style(&id_short).dim(),
        style("⚠ interrupted").yellow().bold(),
        model_part,
        account_part,
        (state.sent_bytes as f64) / 1024.0,
        (state.recv_bytes as f64) / 1024.0,
        elapsed,
      );
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

//! TTY progress display for in-flight requests.
//!
//! Implements [`EventHandler`] driving an [`indicatif::MultiProgress`].
//! One bar per request_id; updated on lifecycle events; finalised on
//! `RequestCompleted` and persisted in the scrollback.
//!
//! Only registered when stdout is a TTY (see `serve.rs` / `proxy.rs`).

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use llm_core::event::{Event, EventHandler};
use std::collections::HashMap;
use std::time::Instant;

struct BarState {
  bar: ProgressBar,
  started: Instant,
  model: String,
  account: String,
  attempt: u32,
  sent_bytes: u64,
  recv_bytes: u64,
}

impl BarState {
  fn id_short(request_id: &str) -> String {
    request_id.chars().take(8).collect()
  }

  fn render_message(&self, request_id: &str) -> String {
    let elapsed = self.started.elapsed().as_secs_f64();
    let speed_kbs = if elapsed > 0.05 {
      (self.recv_bytes as f64) / 1024.0 / elapsed
    } else {
      0.0
    };
    let attempt_tag = if self.attempt > 0 {
      format!(" [retry {}]", self.attempt)
    } else {
      String::new()
    };
    format!(
      "[{}] {:<24} {:<14}{} sent={:>5.1}kB recv={:>6.1}kB @ {:>6.1}kB/s elapsed={:>4.1}s",
      Self::id_short(request_id),
      truncate(&self.model, 24),
      truncate(&self.account, 14),
      attempt_tag,
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

pub struct ProgressEventHandler {
  multi: MultiProgress,
  bars: HashMap<String, BarState>,
  /// Style for in-flight bars (spinner + dynamic message).
  style: ProgressStyle,
}

impl ProgressEventHandler {
  pub fn new() -> Self {
    let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stdout());
    let style = ProgressStyle::with_template("{spinner:.cyan} {msg}")
      .unwrap_or_else(|_| ProgressStyle::default_spinner())
      .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");
    Self {
      multi,
      bars: HashMap::new(),
      style,
    }
  }

  fn refresh(&mut self, request_id: &str) {
    if let Some(state) = self.bars.get(request_id) {
      let msg = state.render_message(request_id);
      state.bar.set_message(msg);
      state.bar.tick();
    }
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
      Event::RequestStarted { request_id, .. } => {
        let bar = self.multi.add(ProgressBar::new_spinner());
        bar.set_style(self.style.clone());
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        let state = BarState {
          bar,
          started: Instant::now(),
          model: String::new(),
          account: String::new(),
          attempt: 0,
          sent_bytes: 0,
          recv_bytes: 0,
        };
        self.bars.insert(request_id.clone(), state);
        self.refresh(request_id);
      }
      Event::RequestParsed {
        request_id,
        attempt,
        account_id,
        model,
        outbound_req,
        ..
      } => {
        if let Some(state) = self.bars.get_mut(request_id) {
          state.model = model.clone();
          state.account = account_id.clone();
          state.attempt = *attempt;
          if let Some(snap) = outbound_req {
            state.sent_bytes = snap.body.len() as u64;
          }
        }
        self.refresh(request_id);
      }
      Event::RequestRetry { request_id, attempt, .. } => {
        if let Some(state) = self.bars.get_mut(request_id) {
          // attempt N just failed; next try will be attempt+1
          state.attempt = attempt + 1;
          state.recv_bytes = 0; // reset for next attempt
        }
        self.refresh(request_id);
      }
      Event::StreamProgress {
        request_id,
        bytes_streamed,
        ..
      } => {
        if let Some(state) = self.bars.get_mut(request_id) {
          state.recv_bytes = *bytes_streamed;
        }
        self.refresh(request_id);
      }
      Event::RequestResult {
        request_id,
        inbound_resp,
        ..
      } => {
        if let Some(state) = self.bars.get_mut(request_id) {
          // Buffered: capture body size as recv bytes.
          let body_len = inbound_resp.body.len() as u64;
          if body_len > state.recv_bytes {
            state.recv_bytes = body_len;
          }
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
          let attempts_tag = if *total_attempts > 1 {
            format!(" attempts={}", total_attempts)
          } else {
            String::new()
          };
          let final_msg = if *success {
            let status = final_status.unwrap_or(0);
            format!(
              "[{}] ✓ {} {:.1}s recv={:.1}kB{}",
              id_short,
              status,
              (*total_latency_ms as f64) / 1000.0,
              (state.recv_bytes as f64) / 1024.0,
              attempts_tag,
            )
          } else {
            let err = error.as_deref().unwrap_or("failed");
            format!(
              "[{}] ✗ {:.1}s{} error={}",
              id_short,
              (*total_latency_ms as f64) / 1000.0,
              attempts_tag,
              truncate(err, 80),
            )
          };
          state.bar.disable_steady_tick();
          state.bar.finish_with_message(final_msg);
        }
      }
      _ => {}
    }
  }

  fn flush(&mut self) {
    // Drop any straggler bars on shutdown.
    for (_, state) in self.bars.drain() {
      state.bar.abandon();
    }
    let _ = self.multi.clear();
  }
}

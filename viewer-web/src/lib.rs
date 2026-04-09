use axum::{
  extract::State,
  response::{Html, IntoResponse},
  routing::get,
  Json, Router,
};
use plugin_defaults::SqliteStore;
use serde::Serialize;

#[derive(Clone)]
pub struct ViewerState {
  pub store: SqliteStore,
}

#[derive(Serialize)]
struct DashboardResponse {
  stats: plugin_defaults::StatsSnapshot,
  events: Vec<plugin_defaults::RecentEvent>,
}

pub fn router(state: ViewerState) -> Router {
  Router::new()
    .route("/", get(index))
    .route("/api/events", get(get_events))
    .route("/api/stats", get(get_stats))
    .route("/api/dashboard", get(get_dashboard))
    .with_state(state)
}

async fn index() -> impl IntoResponse {
  Html(
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>llm-router viewer</title>
    <style>
      body { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; margin: 24px; background: #0f172a; color: #e2e8f0; }
      .card { border: 1px solid #334155; border-radius: 8px; padding: 12px; margin-bottom: 12px; }
      table { width: 100%; border-collapse: collapse; }
      th, td { padding: 6px; border-bottom: 1px solid #1e293b; text-align: left; font-size: 12px; }
      .muted { color: #94a3b8; }
    </style>
  </head>
  <body>
    <h1>llm-router viewer</h1>
    <div id="stats" class="card">loading...</div>
    <div class="card">
      <table>
        <thead>
          <tr><th>id</th><th>request</th><th>type</th><th>route</th><th>status</th><th>latency</th><th>time</th></tr>
        </thead>
        <tbody id="events"></tbody>
      </table>
    </div>
    <script>
      async function refresh() {
        const dashboard = await fetch('./api/dashboard').then(r => r.json());
        const s = dashboard.stats;
        document.getElementById('stats').innerHTML = `
          <div><b>Total Requests:</b> ${s.total_requests}</div>
          <div><b>Errors:</b> ${s.errors}</div>
          <div><b>Avg Latency (ms):</b> ${Number(s.avg_latency_ms).toFixed(2)}</div>
        `;
        const rows = dashboard.events.map(e => `
          <tr>
            <td>${e.id}</td>
            <td class='muted'>${e.request_id}</td>
            <td>${e.event_type}</td>
            <td>${e.route_name || ''}</td>
            <td>${e.status_code || ''}</td>
            <td>${e.latency_ms || ''}</td>
            <td class='muted'>${e.ts}</td>
          </tr>
        `).join('');
        document.getElementById('events').innerHTML = rows;
      }
      refresh();
      setInterval(refresh, 3000);
    </script>
  </body>
</html>
"#,
  )
}

async fn get_events(State(state): State<ViewerState>) -> impl IntoResponse {
  let events = state.store.recent_events(200).unwrap_or_default();
  Json(events)
}

async fn get_stats(State(state): State<ViewerState>) -> impl IntoResponse {
  let stats = state.store.stats().unwrap_or(plugin_defaults::StatsSnapshot {
    total_requests: 0,
    errors: 0,
    avg_latency_ms: 0.0,
  });
  Json(stats)
}

async fn get_dashboard(State(state): State<ViewerState>) -> impl IntoResponse {
  let stats = state.store.stats().unwrap_or(plugin_defaults::StatsSnapshot {
    total_requests: 0,
    errors: 0,
    avg_latency_ms: 0.0,
  });
  let events = state.store.recent_events(200).unwrap_or_default();
  Json(DashboardResponse { stats, events })
}

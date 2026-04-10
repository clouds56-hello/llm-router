import { useState } from "react";
import type { LogEntry } from "../lib/state";

type LogsPageProps = {
  logs: LogEntry[];
  levelFilter: string;
  requestIdFilter: string;
  setLevelFilter: (value: string) => void;
  setRequestIdFilter: (value: string) => void;
};

function oneLine(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function renderMetadata(metadata?: Record<string, string>): string {
  if (!metadata) return "";
  const pairs = Object.entries(metadata)
    .filter(([, value]) => typeof value === "string" && value.length > 0)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([key, value]) => `${key}=${value}`);
  return pairs.join(" ");
}

const LEVEL_BUTTONS: Array<{ value: string; label: string }> = [
  { value: "", label: "ALL" },
  { value: "TRACE", label: "TRACE+" },
  { value: "DEBUG", label: "DEBUG+" },
  { value: "INFO", label: "INFO+" },
  { value: "WARN", label: "WARN+" },
  { value: "ERROR", label: "ERROR" },
];

export function LogsPage(props: LogsPageProps) {
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const toggle = (id: string) => {
    setExpandedId((prev) => (prev === id ? null : id));
  };

  return (
    <section className="grid">
      <article className="card card-wide">
        <h2>Request Logs</h2>
        <div className="logs-controls">
          <div>
            <div>Level</div>
            <div className="logs-level-buttons">
              {LEVEL_BUTTONS.map((level) => (
                <button
                  key={level.label}
                  type="button"
                  className={`logs-level-btn ${props.levelFilter === level.value ? "logs-level-btn-active" : ""}`}
                  onClick={() => props.setLevelFilter(level.value)}
                >
                  {level.label}
                </button>
              ))}
            </div>
          </div>
          <label>
            Request ID
            <div className="row row-tight">
              <input
                value={props.requestIdFilter}
                onChange={(e) => props.setRequestIdFilter(e.target.value)}
                placeholder="req id"
              />
              <button type="button" className="logs-clear-btn" onClick={() => props.setRequestIdFilter("")}>
                Clear
              </button>
            </div>
          </label>
        </div>
        <div className="logs-lines">
          {props.logs.length === 0 ? (
            <p className="muted">No logs</p>
          ) : (
            props.logs.map((log) => (
              <div key={log.id} className="logs-line-wrap">
                <div
                  className="logs-line"
                  onClick={() => toggle(log.id)}
                  role="button"
                  tabIndex={0}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      toggle(log.id);
                    }
                  }}
                >
                  {log.ts} {log.level} {log.target}{" "}
                  {log.request_id ? (
                    <>
                      [
                      <button
                        type="button"
                        className="logs-request-link"
                        onClick={(e) => {
                          e.stopPropagation();
                          props.setRequestIdFilter(log.request_id ?? "");
                        }}
                      >
                        {log.request_id}
                      </button>
                      ]{" "}
                    </>
                  ) : null}
                  {oneLine(log.message)} {renderMetadata(log.metadata)}
                </div>
                {expandedId === log.id ? (
                  <div className="logs-meta">
                    {Object.entries(log.metadata ?? {}).length === 0 ? (
                      <div className="logs-meta-row muted">no metadata</div>
                    ) : (
                      Object.entries(log.metadata ?? {})
                        .sort(([a], [b]) => a.localeCompare(b))
                        .map(([key, value]) => (
                          <div key={`${log.id}-${key}`} className="logs-meta-row">
                            <strong>{key}</strong>: {value}
                          </div>
                        ))
                    )}
                  </div>
                ) : null}
              </div>
            ))
          )}
        </div>
      </article>
    </section>
  );
}

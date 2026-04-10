import { useState } from "react";

import type { ConversationView } from "../lib/state";

type HistoryPageProps = {
  conversations: ConversationView[];
};

function line(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

export function HistoryPage(props: HistoryPageProps) {
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const toggle = (id: string) => {
    setExpandedId((prev) => (prev === id ? null : id));
  };

  return (
    <section className="grid">
      <article className="card card-wide">
        <h2>Chat History</h2>
        <div className="history-list">
          {props.conversations.length === 0 ? (
            <p className="muted">No conversations yet</p>
          ) : (
            props.conversations.map((conv) => (
              <div key={conv.id} className="history-item">
                <button type="button" className="history-head" onClick={() => toggle(conv.id)}>
                  <span className="history-meta">
                    {conv.updated_at} {conv.provider} {conv.model}{" "}
                    {conv.account_id ? `acct=${conv.account_id}` : "acct=default"} msgs={conv.message_count}
                  </span>
                  <span className="history-preview">{line(conv.preview || "(empty)")}</span>
                </button>
                {expandedId === conv.id ? (
                  <div className="history-body">
                    {conv.latest_request_id ? (
                      <div className="history-request-id">request_id: {conv.latest_request_id}</div>
                    ) : null}
                    {conv.messages.length === 0 ? (
                      <div className="muted">No messages</div>
                    ) : (
                      conv.messages.map((m) => (
                        <div key={`${conv.id}-${m.seq}`} className="history-message">
                          <strong>
                            #{m.seq} {m.role}
                          </strong>
                          <div>{m.content_text || "(empty)"}</div>
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

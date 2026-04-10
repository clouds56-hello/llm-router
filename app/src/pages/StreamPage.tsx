type StreamApiMode = "chat_completions" | "responses";

type StreamPageProps = {
  streamInput: string;
  streamOutput: string;
  setStreamInput: (value: string) => void;
  streamAccountKey: string;
  setStreamAccountKey: (value: string) => void;
  streamApiMode: StreamApiMode;
  setStreamApiMode: (value: StreamApiMode) => void;
  streamAccountOptions: Array<{ key: string; label: string; modelName: string }>;
  onRunStreamingTest: () => Promise<void>;
  runAction: (fn: () => Promise<void>) => Promise<void>;
};

export function StreamPage(props: StreamPageProps) {
  return (
    <section className="grid">
      <article className="card card-wide">
        <h2>Streaming Test Console</h2>
        <label>
          Account
          <select value={props.streamAccountKey} onChange={(e) => props.setStreamAccountKey(e.target.value)}>
            {props.streamAccountOptions.length === 0 ? (
              <option value="">No eligible account</option>
            ) : (
              props.streamAccountOptions.map((opt) => (
                <option key={opt.key} value={opt.key}>
                  {opt.label}
                </option>
              ))
            )}
          </select>
        </label>
        <div className="row stream-api-switch" role="group" aria-label="Streaming API mode">
          <button
            type="button"
            className={props.streamApiMode === "chat_completions" ? "tab tab-active" : "tab"}
            onClick={() => props.setStreamApiMode("chat_completions")}
          >
            Chat Completions API
          </button>
          <button
            type="button"
            className={props.streamApiMode === "responses" ? "tab tab-active" : "tab"}
            onClick={() => props.setStreamApiMode("responses")}
          >
            Responses API
          </button>
        </div>
        <textarea value={props.streamInput} onChange={(e) => props.setStreamInput(e.target.value)} rows={3} />
        <button onClick={() => void props.runAction(props.onRunStreamingTest)}>Run Streaming Test</button>
        <pre>{props.streamOutput}</pre>
      </article>
    </section>
  );
}

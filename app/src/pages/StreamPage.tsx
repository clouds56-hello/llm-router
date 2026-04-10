type StreamPageProps = {
  streamInput: string;
  streamOutput: string;
  setStreamInput: (value: string) => void;
  onRunStreamingTest: () => Promise<void>;
  runAction: (fn: () => Promise<void>) => Promise<void>;
};

export function StreamPage(props: StreamPageProps) {
  return (
    <section className="grid">
      <article className="card card-wide">
        <h2>Streaming Test Console</h2>
        <textarea value={props.streamInput} onChange={(e) => props.setStreamInput(e.target.value)} rows={3} />
        <button onClick={() => void props.runAction(props.onRunStreamingTest)}>Run Streaming Test</button>
        <pre>{props.streamOutput}</pre>
      </article>
    </section>
  );
}

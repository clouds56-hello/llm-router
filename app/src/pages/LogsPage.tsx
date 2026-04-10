type LogsPageProps = {
  logs: Array<Record<string, unknown>>;
};

export function LogsPage({ logs }: LogsPageProps) {
  return (
    <section className="grid">
      <article className="card card-wide">
        <h2>Request Logs</h2>
        <pre>{JSON.stringify(logs, null, 2)}</pre>
      </article>
    </section>
  );
}

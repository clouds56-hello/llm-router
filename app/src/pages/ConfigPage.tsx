type ConfigPageProps = {
  config: Record<string, unknown> | null;
};

export function ConfigPage({ config }: ConfigPageProps) {
  return (
    <section className="grid">
      <article className="card card-wide">
        <h2>Active Config</h2>
        <pre>{JSON.stringify(config, null, 2)}</pre>
      </article>
    </section>
  );
}

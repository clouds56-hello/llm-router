import type { ProviderStatus } from "../lib/state";

type StatusPageProps = {
  providers: ProviderStatus[];
  models: Array<Record<string, unknown>>;
};

export function StatusPage({ providers, models }: StatusPageProps) {
  return (
    <section className="grid">
      <article className="card">
        <h2>Provider Status</h2>
        <ul>
          {providers.map((p) => (
            <li key={p.name}>
              <strong>{p.name}</strong> ({p.provider_type}) {p.enabled ? "enabled" : "disabled"}
            </li>
          ))}
        </ul>
      </article>

      <article className="card">
        <h2>Model List</h2>
        <ul>
          {models.map((m, idx) => (
            <li key={idx}>
              {String(m.name)} {"->"} {String(m.provider)}
            </li>
          ))}
        </ul>
      </article>
    </section>
  );
}

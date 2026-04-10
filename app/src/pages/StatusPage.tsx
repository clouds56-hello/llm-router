import type { ProviderStatus } from "../lib/state";
import type { ModelView } from "../lib/state";

type StatusPageProps = {
  providers: ProviderStatus[];
  models: ModelView[];
  onSetProviderEnabled: (provider: string, enabled: boolean) => Promise<void>;
  onSetModelEnabled: (openaiName: string, enabled: boolean) => Promise<void>;
  runAction: (fn: () => Promise<void>) => Promise<void>;
};

export function StatusPage({ providers, models, onSetProviderEnabled, onSetModelEnabled, runAction }: StatusPageProps) {
  return (
    <section className="grid">
      <article className="card">
        <h2>Provider Status</h2>
        <ul>
          {providers.map((p) => (
            <li key={p.name}>
              <div className="row row-tight">
                <strong>{p.name}</strong> ({p.provider_type}) {p.enabled ? "enabled" : "disabled"}
                <button type="button" onClick={() => void runAction(() => onSetProviderEnabled(p.name, !p.enabled))}>
                  {p.enabled ? "Disable" : "Enable"}
                </button>
              </div>
            </li>
          ))}
        </ul>
      </article>

      <article className="card">
        <h2>Model List</h2>
        <ul>
          {models.map((m) => (
            <li key={m.name}>
              <div className="row row-tight">
                {m.name} {"->"} {m.provider} ({m.enabled ? "enabled" : "disabled"})
                <button type="button" onClick={() => void runAction(() => onSetModelEnabled(m.name, !m.enabled))}>
                  {m.enabled ? "Disable" : "Enable"}
                </button>
              </div>
            </li>
          ))}
        </ul>
      </article>
    </section>
  );
}

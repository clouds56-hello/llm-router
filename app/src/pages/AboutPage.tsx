type AboutPageProps = {
  routerBase: string;
};

export function AboutPage({ routerBase }: AboutPageProps) {
  return (
    <section className="grid">
      <article className="card card-wide">
        <h2>About</h2>
        <p>
          <strong>llm-router</strong> desktop dashboard for local OpenAI-compatible routing.
        </p>
        <ul>
          <li>
            Router endpoint: <code>{routerBase}</code>
          </li>
          <li>Providers: account-managed credentials in app</li>
          <li>Diagnostics: status, logs, and live config inspection</li>
        </ul>
        <p className="note">
          Security note: <code>enc2</code> values in <code>credentials.yaml</code> are self-contained obfuscation, not
          strong cryptographic protection.
        </p>
      </article>
    </section>
  );
}

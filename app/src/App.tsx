import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type ProviderStatus = {
  name: string;
  provider_type: string;
  base_url: string;
  enabled: boolean;
  adapter_registered: boolean;
};

type CopilotLoginStart = {
  session_id: string;
  verification_uri: string;
  user_code: string;
  expires_in: number;
  interval: number;
  deployment: unknown;
};

type CopilotComplete = {
  status: string;
  auth_state: unknown | null;
};

const ROUTER_BASE = "http://127.0.0.1:11434";

async function invokeOrFetch<T>(cmd: string, fallbackPath: string): Promise<T> {
  try {
    return await invoke<T>(cmd);
  } catch {
    const res = await fetch(`${ROUTER_BASE}${fallbackPath}`);
    if (!res.ok) {
      throw new Error(`${fallbackPath} failed with ${res.status}`);
    }
    return (await res.json()) as T;
  }
}

export function App() {
  const [providers, setProviders] = useState<ProviderStatus[]>([]);
  const [models, setModels] = useState<Array<Record<string, unknown>>>([]);
  const [config, setConfig] = useState<Record<string, unknown> | null>(null);
  const [logs, setLogs] = useState<Array<Record<string, unknown>>>([]);
  const [loginStatus, setLoginStatus] = useState<Record<string, unknown> | null>(null);
  const [streamInput, setStreamInput] = useState("Write a haiku about Rust async.");
  const [streamOutput, setStreamOutput] = useState("");
  const [deploymentType, setDeploymentType] = useState("github.com");
  const [enterpriseUrl, setEnterpriseUrl] = useState("");
  const [deviceFlow, setDeviceFlow] = useState<CopilotLoginStart | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = async () => {
    try {
      setError(null);
      const [providerData, modelData, configData, logsData, loginData] = await Promise.all([
        invokeOrFetch<ProviderStatus[]>("get_provider_status", "/api/providers/status").then((v) =>
          Array.isArray(v) ? v : (v as unknown as { providers: ProviderStatus[] }).providers
        ),
        invokeOrFetch<Array<Record<string, unknown>>>("get_model_list", "/api/models").then((v) =>
          Array.isArray(v) ? v : (v as unknown as { models: Array<Record<string, unknown>> }).models
        ),
        invoke<Record<string, unknown>>("get_active_config"),
        invoke<Record<string, Array<Record<string, unknown>>>>("get_request_logs"),
        invoke<Record<string, unknown>>("get_login_status"),
      ]);

      setProviders(providerData);
      setModels(modelData);
      setConfig(configData);
      setLogs(logsData.logs ?? []);
      setLoginStatus(loginData);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => {
      void refresh();
    }, 5000);
    return () => clearInterval(timer);
  }, []);

  const startCopilotLogin = async () => {
    const req = {
      deployment_type: deploymentType,
      enterprise_url: deploymentType === "enterprise" ? enterpriseUrl : null,
    };

    const resp = await invoke<CopilotLoginStart>("copilot_login", { request: req });
    setDeviceFlow(resp);
  };

  const completeCopilotLogin = async () => {
    if (!deviceFlow) return;
    const resp = await invoke<CopilotComplete>("copilot_complete_login", {
      request: { session_id: deviceFlow.session_id },
    });

    if (resp.status === "ok") {
      setDeviceFlow(null);
      await refresh();
    } else {
      setError(`copilot login status: ${resp.status}`);
    }
  };

  const logoutCopilot = async () => {
    await invoke("copilot_logout");
    await refresh();
  };

  const runStreamingTest = async () => {
    setStreamOutput("");
    const body = {
      model: "gpt-4.1-mini",
      stream: true,
      messages: [{ role: "user", content: streamInput }],
    };

    const res = await fetch(`${ROUTER_BASE}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });

    if (!res.ok || !res.body) {
      throw new Error(`stream test failed with ${res.status}`);
    }

    const reader = res.body.getReader();
    const decoder = new TextDecoder();

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      setStreamOutput((prev) => prev + decoder.decode(value, { stream: true }));
    }
  };

  return (
    <div className="app">
      <header className="header">
        <h1>llm-router Dashboard</h1>
        <button onClick={() => void refresh()}>Refresh</button>
      </header>

      {error ? <p className="error">{error}</p> : null}

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

        <article className="card">
          <h2>Copilot Login</h2>
          <label>
            Deployment:
            <select value={deploymentType} onChange={(e) => setDeploymentType(e.target.value)}>
              <option value="github.com">GitHub.com</option>
              <option value="enterprise">GitHub Enterprise</option>
            </select>
          </label>
          {deploymentType === "enterprise" ? (
            <label>
              Enterprise URL/domain:
              <input value={enterpriseUrl} onChange={(e) => setEnterpriseUrl(e.target.value)} />
            </label>
          ) : null}
          <div className="row">
            <button onClick={() => void startCopilotLogin()}>Start Login</button>
            <button onClick={() => void completeCopilotLogin()} disabled={!deviceFlow}>
              Complete Login
            </button>
            <button onClick={() => void logoutCopilot()}>Logout</button>
          </div>
          {deviceFlow ? (
            <pre>
{`Visit: ${deviceFlow.verification_uri}
Code: ${deviceFlow.user_code}
Session: ${deviceFlow.session_id}`}
            </pre>
          ) : null}
          <pre>{JSON.stringify(loginStatus, null, 2)}</pre>
        </article>

        <article className="card card-wide">
          <h2>Streaming Test Console</h2>
          <textarea value={streamInput} onChange={(e) => setStreamInput(e.target.value)} rows={3} />
          <button onClick={() => void runStreamingTest()}>Run Streaming Test</button>
          <pre>{streamOutput}</pre>
        </article>

        <article className="card">
          <h2>Active Config</h2>
          <pre>{JSON.stringify(config, null, 2)}</pre>
        </article>

        <article className="card">
          <h2>Request Logs</h2>
          <pre>{JSON.stringify(logs, null, 2)}</pre>
        </article>
      </section>
    </div>
  );
}

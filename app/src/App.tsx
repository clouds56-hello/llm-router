import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type ProviderStatus = {
  name: string;
  provider_type: string;
  base_url: string;
  enabled: boolean;
  adapter_registered: boolean;
};

type AccountView = {
  provider: string;
  id: string;
  label: string;
  auth_type?: string;
  is_default: boolean;
  enabled: boolean;
  meta: Record<string, string>;
  secret_keys: string[];
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
  const [accounts, setAccounts] = useState<AccountView[]>([]);
  const [models, setModels] = useState<Array<Record<string, unknown>>>([]);
  const [config, setConfig] = useState<Record<string, unknown> | null>(null);
  const [logs, setLogs] = useState<Array<Record<string, unknown>>>([]);
  const [loginStatus, setLoginStatus] = useState<Record<string, unknown> | null>(null);
  const [streamInput, setStreamInput] = useState("Write a haiku about Rust async.");
  const [streamOutput, setStreamOutput] = useState("");
  const [deploymentType, setDeploymentType] = useState("github.com");
  const [enterpriseUrl, setEnterpriseUrl] = useState("");
  const [deviceFlow, setDeviceFlow] = useState<CopilotLoginStart | null>(null);
  const [connectProvider, setConnectProvider] = useState("openai");
  const [connectAccountId, setConnectAccountId] = useState("");
  const [connectLabel, setConnectLabel] = useState("");
  const [connectAuthType, setConnectAuthType] = useState("bearer");
  const [connectApiKey, setConnectApiKey] = useState("");
  const [renameAccountId, setRenameAccountId] = useState("");
  const [renameLabel, setRenameLabel] = useState("");
  const [error, setError] = useState<string | null>(null);

  const providerNames = useMemo(() => {
    const names = providers.map((p) => p.name);
    return names.length > 0 ? names : ["openai", "deepseek", "claude", "github_copilot"];
  }, [providers]);

  const accountsByProvider = useMemo(() => {
    const grouped: Record<string, AccountView[]> = {};
    for (const account of accounts) {
      if (!grouped[account.provider]) grouped[account.provider] = [];
      grouped[account.provider].push(account);
    }
    for (const key of Object.keys(grouped)) {
      grouped[key].sort((a, b) => a.label.localeCompare(b.label));
    }
    return grouped;
  }, [accounts]);

  const refresh = async () => {
    try {
      setError(null);
      const [providerData, accountData, modelData, configData, logsData, loginData] = await Promise.all([
        invokeOrFetch<ProviderStatus[]>("get_provider_status", "/api/providers/status").then((v) =>
          Array.isArray(v) ? v : (v as unknown as { providers: ProviderStatus[] }).providers
        ),
        invoke<AccountView[]>("list_accounts"),
        invokeOrFetch<Array<Record<string, unknown>>>("get_model_list", "/api/models").then((v) =>
          Array.isArray(v) ? v : (v as unknown as { models: Array<Record<string, unknown>> }).models
        ),
        invoke<Record<string, unknown>>("get_active_config"),
        invoke<Record<string, Array<Record<string, unknown>>>>("get_request_logs"),
        invoke<Record<string, unknown>>("get_login_status"),
      ]);

      setProviders(providerData);
      setAccounts(accountData);
      setModels(modelData);
      setConfig(configData);
      setLogs(logsData.logs ?? []);
      setLoginStatus(loginData);
      if (!providerData.find((p) => p.name === connectProvider) && providerData.length > 0) {
        setConnectProvider(providerData[0].name);
      }
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

  const connectApiKeyAccount = async () => {
    if (!connectApiKey.trim()) {
      throw new Error("API key is required");
    }
    await invoke("connect_account", {
      request: {
        provider: connectProvider,
        account_id: connectAccountId.trim() || null,
        label: connectLabel.trim() || null,
        auth_type: connectAuthType.trim() || null,
        secrets: {
          api_key: connectApiKey,
        },
        set_default: true,
        enabled: true,
      },
    });

    setConnectApiKey("");
    setConnectAccountId("");
    if (!connectLabel.trim()) {
      setConnectLabel("");
    }
    await refresh();
  };

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

  const setDefaultAccount = async (provider: string, accountId: string) => {
    await invoke("set_default_account", {
      request: { provider, account_id: accountId },
    });
    await refresh();
  };

  const disconnectAccount = async (provider: string, accountId: string) => {
    await invoke("disconnect_account", {
      request: { provider, account_id: accountId },
    });
    await refresh();
  };

  const toggleAccount = async (account: AccountView) => {
    await invoke("update_account", {
      request: {
        provider: account.provider,
        account_id: account.id,
        enabled: !account.enabled,
      },
    });
    await refresh();
  };

  const renameAccount = async () => {
    if (!renameAccountId.trim() || !renameLabel.trim()) {
      throw new Error("Choose account id and new label");
    }

    const [provider, accountId] = renameAccountId.split("::");
    if (!provider || !accountId) {
      throw new Error("Invalid account selection");
    }

    await invoke("update_account", {
      request: {
        provider,
        account_id: accountId,
        label: renameLabel,
      },
    });

    setRenameLabel("");
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

  const runAction = async (fn: () => Promise<void>) => {
    try {
      setError(null);
      await fn();
    } catch (e) {
      setError((e as Error).message);
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
        <article className="card card-wide">
          <h2>Credential Accounts</h2>

          <div className="account-connect">
            <h3>Connect API Key Account</h3>
            <label>
              Provider
              <select value={connectProvider} onChange={(e) => setConnectProvider(e.target.value)}>
                {providerNames.map((name) => (
                  <option key={name} value={name}>
                    {name}
                  </option>
                ))}
              </select>
            </label>
            <label>
              Account ID (optional)
              <input value={connectAccountId} onChange={(e) => setConnectAccountId(e.target.value)} />
            </label>
            <label>
              Label (optional)
              <input value={connectLabel} onChange={(e) => setConnectLabel(e.target.value)} />
            </label>
            <label>
              Auth Type
              <input value={connectAuthType} onChange={(e) => setConnectAuthType(e.target.value)} />
            </label>
            <label>
              API Key
              <input
                type="password"
                value={connectApiKey}
                onChange={(e) => setConnectApiKey(e.target.value)}
                placeholder="sk-..."
              />
            </label>
            <button onClick={() => void runAction(connectApiKeyAccount)}>Connect Account</button>
            <p className="note">API keys are stored in <code>credentials.yaml</code> using <code>enc2:</code> self-contained obfuscation.</p>
          </div>

          <div className="account-rename">
            <h3>Rename Account</h3>
            <label>
              Account
              <select value={renameAccountId} onChange={(e) => setRenameAccountId(e.target.value)}>
                <option value="">Select account</option>
                {accounts.map((account) => (
                  <option key={`${account.provider}::${account.id}`} value={`${account.provider}::${account.id}`}>
                    {account.provider} / {account.id}
                  </option>
                ))}
              </select>
            </label>
            <label>
              New Label
              <input value={renameLabel} onChange={(e) => setRenameLabel(e.target.value)} />
            </label>
            <button onClick={() => void runAction(renameAccount)}>Update Label</button>
          </div>

          <div className="accounts-grid">
            {providerNames.map((providerName) => {
              const items = accountsByProvider[providerName] ?? [];
              return (
                <section key={providerName} className="account-provider">
                  <h3>{providerName}</h3>
                  {items.length === 0 ? (
                    <p className="muted">No accounts</p>
                  ) : (
                    <ul>
                      {items.map((account) => (
                        <li key={account.id}>
                          <div className="row row-tight">
                            <strong>{account.label}</strong>
                            <span>id: {account.id}</span>
                            <span>{account.auth_type ?? "auth_type: unset"}</span>
                            <span>{account.enabled ? "enabled" : "disabled"}</span>
                            {account.is_default ? <span className="badge">default</span> : null}
                          </div>
                          <div className="row row-tight">
                            <button onClick={() => void runAction(() => setDefaultAccount(providerName, account.id))}>
                              Set Default
                            </button>
                            <button onClick={() => void runAction(() => toggleAccount(account))}>
                              {account.enabled ? "Disable" : "Enable"}
                            </button>
                            <button onClick={() => void runAction(() => disconnectAccount(providerName, account.id))}>
                              Disconnect
                            </button>
                          </div>
                          <small>secrets: {account.secret_keys.join(", ") || "none"}</small>
                        </li>
                      ))}
                    </ul>
                  )}
                </section>
              );
            })}
          </div>
        </article>

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
            <button onClick={() => void runAction(startCopilotLogin)}>Start Login</button>
            <button onClick={() => void runAction(completeCopilotLogin)} disabled={!deviceFlow}>
              Complete Login
            </button>
            <button onClick={() => void runAction(logoutCopilot)}>Logout</button>
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
          <button onClick={() => void runAction(runStreamingTest)}>Run Streaming Test</button>
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

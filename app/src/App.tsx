import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { AccountsPage } from "./pages/AccountsPage";
import { AboutPage } from "./pages/AboutPage";
import { ConfigPage } from "./pages/ConfigPage";
import { LogsPage } from "./pages/LogsPage";
import { StatusPage } from "./pages/StatusPage";
import { StreamPage } from "./pages/StreamPage";
import {
  type AccountView,
  type CopilotComplete,
  type CopilotLoginStart,
  type ProviderStatus,
  type RemovedAccountUndo,
  type TabId,
  ROUTER_BASE,
  TABS,
  getCredentialAccountSnapshot,
  groupAccountsByProvider,
  persistTab,
  readStoredTab,
} from "./lib/state";

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
  const [activeTab, setActiveTab] = useState<TabId>(readStoredTab);
  const [providers, setProviders] = useState<ProviderStatus[]>([]);
  const [accounts, setAccounts] = useState<AccountView[]>([]);
  const [models, setModels] = useState<Array<Record<string, unknown>>>([]);
  const [config, setConfig] = useState<Record<string, unknown> | null>(null);
  const [logs, setLogs] = useState<Array<Record<string, unknown>>>([]);
  const [streamInput, setStreamInput] = useState("Write a haiku about Rust async.");
  const [streamOutput, setStreamOutput] = useState("");
  const [deploymentType, setDeploymentType] = useState("github.com");
  const [enterpriseUrl, setEnterpriseUrl] = useState("");
  const [deviceFlow, setDeviceFlow] = useState<CopilotLoginStart | null>(null);
  const [removedUndos, setRemovedUndos] = useState<RemovedAccountUndo[]>([]);
  const [error, setError] = useState<string | null>(null);

  const providerNames = useMemo(() => {
    const ordered = providers.map((p) => p.name);
    const extras = accounts
      .map((a) => a.provider)
      .filter((name, idx, arr) => arr.indexOf(name) === idx && !ordered.includes(name));
    const merged = [...ordered, ...extras];
    return merged.length > 0 ? merged : ["openai", "deepseek", "claude", "github_copilot"];
  }, [providers, accounts]);

  const accountsByProvider = useMemo(() => groupAccountsByProvider(accounts), [accounts]);

  useEffect(() => {
    persistTab(activeTab);
  }, [activeTab]);

  const refresh = async () => {
    try {
      setError(null);
      const [providerData, accountData, modelData, configData, logsData] = await Promise.all([
        invokeOrFetch<ProviderStatus[]>("get_provider_status", "/api/providers/status").then((v) =>
          Array.isArray(v) ? v : (v as unknown as { providers: ProviderStatus[] }).providers
        ),
        invoke<AccountView[]>("list_accounts"),
        invokeOrFetch<Array<Record<string, unknown>>>("get_model_list", "/api/models").then((v) =>
          Array.isArray(v) ? v : (v as unknown as { models: Array<Record<string, unknown>> }).models
        ),
        invoke<Record<string, unknown>>("get_active_config"),
        invoke<Record<string, Array<Record<string, unknown>>>>("get_request_logs"),
      ]);

      setProviders(providerData);
      setAccounts(accountData);
      setModels(modelData);
      setConfig(configData);
      setLogs(logsData.logs ?? []);
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
    // console.log("copilot login response", resp);
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

  const startOauthForProvider = async (provider: string) => {
    if (!provider.toLowerCase().includes("github")) {
      throw new Error("OAuth is currently available only for GitHub providers");
    }
    await startCopilotLogin();
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

  const addApiAccount = async (input: {
    provider: string;
    accountId?: string;
    label?: string;
    authType?: string;
    apiKey: string;
  }) => {
    if (!input.apiKey.trim()) {
      throw new Error("API key is required");
    }

    await invoke("connect_account", {
      request: {
        provider: input.provider,
        account_id: input.accountId ?? null,
        label: input.label ?? null,
        auth_type: input.authType ?? null,
        secrets: { api_key: input.apiKey },
        set_default: true,
        enabled: true,
      },
    });

    await refresh();
  };

  const modifyAccount = async (input: {
    provider: string;
    accountId: string;
    label?: string;
    authType?: string;
    enabled?: boolean;
    apiKey?: string;
    oauthAccessToken?: string;
    refreshApiKey?: boolean;
  }) => {
    const setSecrets: Record<string, string> = {};
    if (input.apiKey) {
      setSecrets.api_key = input.apiKey;
    }
    if (input.oauthAccessToken) {
      setSecrets.oauth_access_token = input.oauthAccessToken;
      if (input.refreshApiKey) {
        setSecrets.api_key = input.oauthAccessToken;
      }
    }
    await invoke("update_account", {
      request: {
        provider: input.provider,
        account_id: input.accountId,
        label: input.label ?? null,
        auth_type: input.authType ?? null,
        enabled: input.enabled ?? null,
        set_secrets: Object.keys(setSecrets).length > 0 ? setSecrets : null,
      },
    });
    await refresh();
  };

  const removeAccount = async (provider: string, account: AccountView, originalIndex: number) => {
    const snapshot = getCredentialAccountSnapshot(config, provider, account.id);

    await invoke("disconnect_account", {
      request: { provider, account_id: account.id },
    });

    await refresh();

    if (!snapshot) {
      return;
    }

    const key = `${provider}::${account.id}`;
    setRemovedUndos((prev) => [
      ...prev.filter((row) => row.key !== key),
      { key, provider, originalIndex, snapshot },
    ]);
  };

  const undoRemove = async (undo: RemovedAccountUndo) => {
    await invoke("connect_account", {
      request: {
        provider: undo.snapshot.provider,
        account_id: undo.snapshot.id,
        label: undo.snapshot.label,
        auth_type: undo.snapshot.auth_type ?? null,
        secrets: undo.snapshot.secrets,
        meta: undo.snapshot.meta,
        set_default: undo.snapshot.is_default,
        enabled: undo.snapshot.enabled,
      },
    });

    setRemovedUndos((prev) => prev.filter((row) => row.key !== undo.key));
    await refresh();
  };

  const renderActiveTab = () => {
    switch (activeTab) {
      case "accounts":
        return (
          <AccountsPage
            providerNames={providerNames}
            accountsByProvider={accountsByProvider}
            removedUndos={removedUndos}
            deploymentType={deploymentType}
            setDeploymentType={setDeploymentType}
            enterpriseUrl={enterpriseUrl}
            setEnterpriseUrl={setEnterpriseUrl}
            deviceFlow={deviceFlow}
            onAddApiAccount={addApiAccount}
            onUpdateAccount={modifyAccount}
            onRemoveAccount={removeAccount}
            onUndoRemove={undoRemove}
            onStartOauthForProvider={startOauthForProvider}
            onCompleteCopilotLogin={completeCopilotLogin}
            runAction={runAction}
          />
        );
      case "status":
        return <StatusPage providers={providers} models={models} />;
      case "stream":
        return (
          <StreamPage
            streamInput={streamInput}
            streamOutput={streamOutput}
            setStreamInput={setStreamInput}
            onRunStreamingTest={runStreamingTest}
            runAction={runAction}
          />
        );
      case "logs":
        return <LogsPage logs={logs} />;
      case "config":
        return <ConfigPage config={config} />;
      case "about":
        return <AboutPage routerBase={ROUTER_BASE} />;
      default:
        return null;
    }
  };

  return (
    <div className="app">
      <header className="header">
        <h1>llm-router Dashboard</h1>
        <button onClick={() => void refresh()}>Refresh</button>
      </header>

      <nav className="tabs" aria-label="Dashboard tabs">
        {TABS.map((tab) => (
          <button
            key={tab.id}
            className={`tab ${activeTab === tab.id ? "tab-active" : ""}`}
            onClick={() => setActiveTab(tab.id)}
            type="button"
          >
            {tab.label}
          </button>
        ))}
      </nav>

      {error ? <p className="error">{error}</p> : null}

      {renderActiveTab()}
    </div>
  );
}

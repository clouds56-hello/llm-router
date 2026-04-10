import { useCallback, useEffect, useMemo, useState } from "react";
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
  type LogEntry,
  type ModelView,
  type ProviderStatus,
  type RemovedAccountUndo,
  type TabId,
  ROUTER_BASE_DEFAULT,
  TABS,
  getCredentialAccountSnapshot,
  groupAccountsByProvider,
  persistTab,
  readStoredTab,
} from "./lib/state";

const LOG_LEVEL_RANK: Record<string, number> = {
  TRACE: 10,
  DEBUG: 20,
  INFO: 30,
  WARN: 40,
  ERROR: 50,
};

async function invokeOrFetch<T>(
  cmd: string,
  fallbackPath: string,
  routerBase: string,
  invokeArgs?: Record<string, unknown>
): Promise<T> {
  try {
    return await invoke<T>(cmd, invokeArgs);
  } catch {
    const res = await fetch(`${routerBase}${fallbackPath}`);
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
  const [models, setModels] = useState<ModelView[]>([]);
  const [config, setConfig] = useState<Record<string, unknown> | null>(null);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [logLevelFilter, setLogLevelFilter] = useState("");
  const [logRequestIdFilter, setLogRequestIdFilter] = useState("");
  const [streamInput, setStreamInput] = useState("Write a haiku about Rust async.");
  const [streamOutput, setStreamOutput] = useState("");
  const [routerBase, setRouterBase] = useState(ROUTER_BASE_DEFAULT);
  const [streamAccountKey, setStreamAccountKey] = useState("");
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

  const modelNameByProvider = useMemo(() => {
    const out: Record<string, string> = {};
    for (const row of models) {
      const provider = row.provider;
      const name = row.name;
      if (!row.enabled) {
        continue;
      }
      if (typeof provider === "string" && typeof name === "string" && !out[provider]) {
        out[provider] = name;
      }
    }
    return out;
  }, [models]);

  const streamAccountOptions = useMemo(() => {
    const out: Array<{ key: string; provider: string; accountId: string; label: string; modelName: string }> = [];
    for (const providerName of providerNames) {
      const modelName = modelNameByProvider[providerName];
      if (!modelName) {
        continue;
      }
      const providerAccounts = accountsByProvider[providerName] ?? [];
      for (const account of providerAccounts) {
        out.push({
          key: `${providerName}::${account.id}`,
          provider: providerName,
          accountId: account.id,
          label: `${providerName} / ${account.label}`,
          modelName,
        });
      }
    }
    return out;
  }, [providerNames, modelNameByProvider, accountsByProvider]);

  useEffect(() => {
    if (!streamAccountOptions.find((opt) => opt.key === streamAccountKey)) {
      setStreamAccountKey(streamAccountOptions[0]?.key ?? "");
    }
  }, [streamAccountOptions, streamAccountKey]);

  useEffect(() => {
    persistTab(activeTab);
  }, [activeTab]);

  const refresh = useCallback(async () => {
    try {
      setError(null);
      const logsReq = {
        limit: 500,
        level: null,
        request_id: logRequestIdFilter.trim() || null,
      };
      const logsFallbackPath = `/api/logs?${new URLSearchParams(
        Object.entries(logsReq)
          .filter(([, value]) => value !== null)
          .map(([key, value]) => [key, String(value)])
      ).toString()}`;
      const [providerData, accountData, modelData, configData, logsData] = await Promise.all([
        invokeOrFetch<ProviderStatus[]>("get_provider_status", "/api/providers/status", routerBase).then((v) =>
          Array.isArray(v) ? v : (v as unknown as { providers: ProviderStatus[] }).providers
        ),
        invoke<AccountView[]>("list_accounts"),
        invokeOrFetch<ModelView[]>("get_model_list", "/api/models", routerBase).then((v) =>
          Array.isArray(v) ? v : (v as unknown as { models: ModelView[] }).models
        ),
        invoke<Record<string, unknown>>("get_active_config"),
        invokeOrFetch<Record<string, LogEntry[]>>("get_request_logs", logsFallbackPath, routerBase, {
          request: logsReq,
        }),
      ]);

      setProviders(providerData);
      setAccounts(accountData);
      setModels(modelData);
      setConfig(configData);
      setLogs(logsData.logs ?? []);

      const routerState = await invoke<{ running: boolean; addr: string | null }>("get_router_state");
      if (routerState.running && routerState.addr) {
        setRouterBase(`http://${routerState.addr}`);
      }
    } catch (e) {
      setError((e as Error).message);
    }
  }, [logRequestIdFilter, routerBase]);

  const visibleLogs = useMemo(() => {
    const minRank = LOG_LEVEL_RANK[logLevelFilter] ?? 0;
    if (!minRank) return logs;
    return logs.filter((log) => (LOG_LEVEL_RANK[String(log.level).toUpperCase()] ?? 0) >= minRank);
  }, [logs, logLevelFilter]);

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => {
      void refresh();
    }, 5000);
    return () => clearInterval(timer);
  }, [refresh]);

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
    const selected = streamAccountOptions.find((opt) => opt.key === streamAccountKey);
    if (!selected) {
      throw new Error("Select an account with an available model first");
    }

    setStreamOutput("");
    const body = {
      model: selected.modelName,
      stream: true,
      messages: [{ role: "user", content: streamInput }],
    };

    const res = await fetch(`${routerBase}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-llm-router-account-id": selected.accountId,
      },
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

  const testAccount = async (provider: string, accountId: string) => {
    const modelName = modelNameByProvider[provider];
    if (!modelName) {
      throw new Error(`No routed model found for provider '${provider}'`);
    }

    const res = await fetch(`${routerBase}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-llm-router-account-id": accountId,
      },
      body: JSON.stringify({
        model: modelName,
        stream: false,
        messages: [{ role: "user", content: "Reply with: account test ok" }],
      }),
    });

    if (!res.ok) {
      const details = await res.text();
      throw new Error(`Account test failed (${res.status}): ${details || "unknown error"}`);
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

  const setProviderEnabled = async (provider: string, enabled: boolean) => {
    await invoke("set_provider_enabled", {
      request: { provider, enabled },
    });
    await refresh();
  };

  const setModelEnabled = async (openaiName: string, enabled: boolean) => {
    await invoke("set_model_enabled", {
      request: { openai_name: openaiName, enabled },
    });
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
            onTestAccount={testAccount}
            onRemoveAccount={removeAccount}
            onUndoRemove={undoRemove}
            onStartOauthForProvider={startOauthForProvider}
            onCompleteCopilotLogin={completeCopilotLogin}
            runAction={runAction}
          />
        );
      case "status":
        return (
          <StatusPage
            providers={providers}
            models={models}
            onSetProviderEnabled={setProviderEnabled}
            onSetModelEnabled={setModelEnabled}
            runAction={runAction}
          />
        );
      case "stream":
        return (
          <StreamPage
            streamInput={streamInput}
            streamOutput={streamOutput}
            setStreamInput={setStreamInput}
            streamAccountKey={streamAccountKey}
            setStreamAccountKey={setStreamAccountKey}
            streamAccountOptions={streamAccountOptions.map((opt) => ({
              key: opt.key,
              label: `${opt.label} (${opt.modelName})`,
              modelName: opt.modelName,
            }))}
            onRunStreamingTest={runStreamingTest}
            runAction={runAction}
          />
        );
      case "logs":
        return (
          <LogsPage
            logs={visibleLogs}
            levelFilter={logLevelFilter}
            requestIdFilter={logRequestIdFilter}
            setLevelFilter={setLogLevelFilter}
            setRequestIdFilter={setLogRequestIdFilter}
          />
        );
      case "config":
        return <ConfigPage config={config} />;
      case "about":
        return <AboutPage routerBase={routerBase} />;
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

import { type ReactNode, useMemo, useState } from "react";

import type { AccountInformationView, AccountQuotaItem, AccountView, RemovedAccountUndo } from "../lib/state";

type AccountsPageProps = {
  providerNames: string[];
  accountsByProvider: Record<string, AccountView[]>;
  accountInformation: AccountInformationView[];
  removedUndos: RemovedAccountUndo[];
  deploymentType: string;
  setDeploymentType: (value: string) => void;
  enterpriseUrl: string;
  setEnterpriseUrl: (value: string) => void;
  deviceFlow: { verification_uri: string; user_code: string; session_id: string } | null;
  oauthProvider: string | null;
  onAddApiAccount: (input: {
    provider: string;
    accountId?: string;
    label?: string;
    authType?: string;
    apiKey: string;
  }) => Promise<void>;
  onStartOauthForProvider: (provider: string) => Promise<void>;
  onUpdateAccount: (input: {
    provider: string;
    accountId: string;
    label?: string;
    authType?: string;
    enabled?: boolean;
    apiKey?: string;
    oauthAccessToken?: string;
    refreshApiKey?: boolean;
  }) => Promise<void>;
  onTestAccount: (provider: string, accountId: string) => Promise<void>;
  onRemoveAccount: (provider: string, account: AccountView, originalIndex: number) => Promise<void>;
  onUndoRemove: (undo: RemovedAccountUndo) => Promise<void>;
  onCompleteCopilotLogin: () => Promise<void>;
  runAction: (fn: () => Promise<void>) => Promise<void>;
};

type AddMode = "api_key" | "oauth";

function Modal(props: { title: string; onClose: () => void; children: ReactNode }) {
  return (
    <div className="modal-backdrop" role="dialog" aria-modal="true">
      <div className="modal-card">
        <div className="modal-header">
          <h3>{props.title}</h3>
          <button type="button" className="modal-close" onClick={props.onClose}>
            Close
          </button>
        </div>
        {props.children}
      </div>
    </div>
  );
}

export function AccountsPage(props: AccountsPageProps) {
  const [addOpen, setAddOpen] = useState(false);
  const [addMode, setAddMode] = useState<AddMode>("api_key");
  const [addProvider, setAddProvider] = useState(props.providerNames[0] ?? "openai");
  const [addAccountId, setAddAccountId] = useState("");
  const [addLabel, setAddLabel] = useState("");
  const [addAuthType, setAddAuthType] = useState("bearer");
  const [addApiKey, setAddApiKey] = useState("");

  const [editTarget, setEditTarget] = useState<AccountView | null>(null);
  const [editLabel, setEditLabel] = useState("");
  const [editAuthType, setEditAuthType] = useState("");
  const [editEnabled, setEditEnabled] = useState(true);
  const [editApiKey, setEditApiKey] = useState("");
  const [editOauthAccessToken, setEditOauthAccessToken] = useState("");

  const undosByProvider = useMemo(() => {
    const out: Record<string, RemovedAccountUndo[]> = {};
    for (const undo of props.removedUndos) {
      if (!out[undo.provider]) out[undo.provider] = [];
      out[undo.provider].push(undo);
    }
    for (const key of Object.keys(out)) {
      out[key].sort((a, b) => a.originalIndex - b.originalIndex);
    }
    return out;
  }, [props.removedUndos]);

  const accountInfoByKey = useMemo(() => {
    const out: Record<string, AccountInformationView> = {};
    for (const row of props.accountInformation) {
      out[`${row.provider}::${row.account_id}`] = row;
    }
    return out;
  }, [props.accountInformation]);

  const dashboardStats = useMemo(() => {
    const allAccounts = Object.values(props.accountsByProvider).flat();
    const enabled = allAccounts.filter((row) => row.enabled).length;
    const oauth = allAccounts.filter(
      (row) => row.meta?.oauth === "true" || row.secret_keys.includes("oauth_access_token")
    ).length;
    return {
      total: allAccounts.length,
      enabled,
      disabled: allAccounts.length - enabled,
      oauth,
      providers: props.providerNames.length,
    };
  }, [props.accountsByProvider, props.providerNames]);

  const openAdd = () => {
    const provider = props.providerNames[0] ?? "openai";
    const oauthDefault = provider.toLowerCase().includes("github") || provider === "codex";
    setAddOpen(true);
    setAddProvider(provider);
    setAddMode(oauthDefault ? "oauth" : "api_key");
    setAddAccountId("");
    setAddLabel("");
    setAddAuthType("bearer");
    setAddApiKey("");
  };

  const openAddForProvider = (provider: string) => {
    const oauthDefault = provider.toLowerCase().includes("github") || provider === "codex";
    setAddOpen(true);
    setAddProvider(provider);
    setAddMode(oauthDefault ? "oauth" : "api_key");
    setAddAccountId("");
    setAddLabel("");
    setAddAuthType("bearer");
    setAddApiKey("");
  };

  const openEdit = (account: AccountView) => {
    setEditTarget(account);
    setEditLabel(account.label);
    setEditAuthType(account.auth_type ?? "");
    setEditEnabled(account.enabled);
    setEditApiKey("");
    setEditOauthAccessToken("");
  };

  const submitAdd = async () => {
    if (addMode === "oauth") {
      await props.onStartOauthForProvider(addProvider);
      return;
    }

    await props.onAddApiAccount({
      provider: addProvider,
      accountId: addAccountId.trim() || undefined,
      label: addLabel.trim() || undefined,
      authType: addAuthType.trim() || undefined,
      apiKey: addApiKey,
    });

    setAddOpen(false);
  };

  const completeOauthAndClose = async () => {
    await props.onCompleteCopilotLogin();
    setAddOpen(false);
  };

  const isEditOauthAccount =
    editTarget !== null &&
    (editTarget.meta?.oauth === "true" || editTarget.secret_keys.includes("oauth_access_token"));

  const refreshApiKeyFromOauthToken = async () => {
    if (!editTarget) return;

    await props.onUpdateAccount({
      provider: editTarget.provider,
      accountId: editTarget.id,
      oauthAccessToken: editOauthAccessToken.trim() || undefined,
      refreshApiKey: true,
    });
  };

  const submitEdit = async () => {
    if (!editTarget) return;

    await props.onUpdateAccount({
      provider: editTarget.provider,
      accountId: editTarget.id,
      label: editLabel.trim() || undefined,
      authType: editAuthType.trim() || undefined,
      enabled: editEnabled,
      apiKey: editApiKey.trim() || undefined,
      oauthAccessToken: editOauthAccessToken.trim() || undefined,
    });

    setEditTarget(null);
  };

  return (
    <section className="grid">
      <article className="card card-wide accounts-dashboard">
        <div className="accounts-dashboard-head">
          <div>
            <h2>Account Dashboard</h2>
            <p className="muted">Manage API keys, OAuth sessions, and provider readiness in one place.</p>
          </div>
          <button type="button" className="add-account-btn" onClick={openAdd}>
            Add Account
          </button>
        </div>

        <div className="accounts-kpis">
          <div className="accounts-kpi-card">
            <span>Total Accounts</span>
            <strong>{dashboardStats.total}</strong>
          </div>
          <div className="accounts-kpi-card">
            <span>Active</span>
            <strong>{dashboardStats.enabled}</strong>
          </div>
          <div className="accounts-kpi-card">
            <span>OAuth Linked</span>
            <strong>{dashboardStats.oauth}</strong>
          </div>
          <div className="accounts-kpi-card">
            <span>Providers</span>
            <strong>{dashboardStats.providers}</strong>
          </div>
        </div>

        <div className="accounts-grid">
          {props.providerNames.map((providerName) => {
            const items = props.accountsByProvider[providerName] ?? [];
            const undos = undosByProvider[providerName] ?? [];

            const rows: Array<{ kind: "account"; account: AccountView } | { kind: "undo"; undo: RemovedAccountUndo }> =
              [];
            let undoIdx = 0;
            for (let idx = 0; idx <= items.length; idx += 1) {
              while (undoIdx < undos.length && undos[undoIdx].originalIndex === idx) {
                rows.push({ kind: "undo", undo: undos[undoIdx] });
                undoIdx += 1;
              }
              if (idx < items.length) {
                rows.push({ kind: "account", account: items[idx] });
              }
            }
            while (undoIdx < undos.length) {
              rows.push({ kind: "undo", undo: undos[undoIdx] });
              undoIdx += 1;
            }

            return (
              <section key={providerName} className="account-provider">
                <div className="provider-head">
                  <div>
                    <h3>{providerName}</h3>
                    <p className="muted">
                      {items.length} account{items.length === 1 ? "" : "s"} configured
                    </p>
                  </div>
                  <button type="button" className="add-account-btn" onClick={() => openAddForProvider(providerName)}>
                    Add
                  </button>
                </div>

                {rows.length === 0 ? (
                  <div className="account-empty">
                    <p className="muted">No accounts for this provider yet.</p>
                  </div>
                ) : (
                  <ul className="account-list">
                    {rows.map((row, index) => {
                      if (row.kind === "undo") {
                        return (
                          <li key={row.undo.key} className="undo-row">
                            <div className="account-item-row">
                              <span>Account removed</span>
                              <button
                                type="button"
                                className="btn-ghost"
                                onClick={() => void props.runAction(() => props.onUndoRemove(row.undo))}
                              >
                                Undo
                              </button>
                            </div>
                          </li>
                        );
                      }

                      const account = row.account;
                      const originalIndex = index;
                      const info = accountInfoByKey[`${providerName}::${account.id}`];
                      const quotaItems: AccountQuotaItem[] = parseQuotaItems(info?.quota);

                      return (
                        <li
                          key={account.id}
                          className="account-item account-item-clickable"
                          role="button"
                          tabIndex={0}
                          onClick={() => openEdit(account)}
                          onKeyDown={(event) => {
                            if (event.key === "Enter" || event.key === " ") {
                              event.preventDefault();
                              openEdit(account);
                            }
                          }}
                          aria-label={`Open details for ${account.label}`}
                        >
                          <div className="account-item-head">
                            <div>
                              <strong>{info?.name || info?.email || account.label}</strong>
                              <p className="muted">{account.id}</p>
                            </div>
                            <div className="account-tags">
                              <span className={`status-dot ${account.enabled ? "status-ok" : "status-off"}`}>
                                {account.enabled ? "enabled" : "disabled"}
                              </span>
                              <span className="status-dot">{account.auth_type ?? "unset auth"}</span>
                              {account.is_default ? <span className="badge">default</span> : null}
                            </div>
                          </div>

                          <div className="account-actions">
                            <button
                              type="button"
                              className="account-icon-btn account-icon-debug"
                              title="Test account"
                              aria-label="Test account"
                              onClick={(event) => {
                                event.stopPropagation();
                                void props.runAction(() => props.onTestAccount(providerName, account.id));
                              }}
                            >
                              ▶
                            </button>
                            <button
                              type="button"
                              className="account-icon-btn account-icon-remove"
                              title="Remove account"
                              aria-label="Remove account"
                              onClick={(event) => {
                                event.stopPropagation();
                                void props.runAction(() =>
                                  props.onRemoveAccount(providerName, account, Math.max(0, originalIndex))
                                );
                              }}
                            >
                              ×
                            </button>
                          </div>

                          {info ? (
                            <div className="account-meta-grid">
                              <span>status: {info.status}</span>
                              {info.plan ? <span>plan: {info.plan}</span> : null}
                              {info.email ? <span>email: {info.email}</span> : null}
                            </div>
                          ) : null}

                          {quotaItems.length > 0 ? (
                            <div className="quota-panel">
                              <small>Quota</small>
                              <ul>
                                {quotaItems.map((q) => {
                                  const remainingPercent = computeQuotaRemainingPercent(q);
                                  return (
                                    <li key={q.name}>
                                      <div className="quota-line">
                                        <span>{q.name}</span>
                                        <span>
                                          remaining: {formatQuotaNumber(q.remaining)} / {formatQuotaNumber(q.total)}
                                        </span>
                                      </div>
                                      {typeof remainingPercent === "number" ? (
                                        <div
                                          className="quota-bar"
                                          role="img"
                                          aria-label={`${q.name} remaining ${remainingPercent}%`}
                                        >
                                          <span style={{ width: `${remainingPercent}%` }} />
                                        </div>
                                      ) : null}
                                    </li>
                                  );
                                })}
                              </ul>
                            </div>
                          ) : null}

                        </li>
                      );
                    })}
                  </ul>
                )}
              </section>
            );
          })}
        </div>

        <p className="muted accounts-footnote">
          Disabled accounts stay configured and can be re-enabled from account details.
          {dashboardStats.disabled > 0 ? ` Currently disabled: ${dashboardStats.disabled}.` : ""}
        </p>
      </article>

      {addOpen ? (
        <Modal title="Add Account" onClose={() => setAddOpen(false)}>
          <label>
            Provider
            <select
              value={addProvider}
              onChange={(e) => {
                const provider = e.target.value;
                setAddProvider(provider);
                if (provider.toLowerCase().includes("github") || provider === "codex") {
                  setAddMode("oauth");
                }
              }}
            >
              {props.providerNames.map((name) => (
                <option key={name} value={name}>
                  {name}
                </option>
              ))}
            </select>
          </label>

          <label>
            Connect method
            <select
              value={addMode}
              onChange={(e) => setAddMode(e.target.value as AddMode)}
              disabled={addProvider.toLowerCase().includes("github") || addProvider === "codex"}
            >
              <option value="api_key">API Key</option>
              <option value="oauth">OAuth</option>
            </select>
          </label>
          {addProvider.toLowerCase().includes("github") || addProvider === "codex" ? (
            <p className="note">
              {addProvider === "codex"
                ? "Codex provider defaults to OAuth connection."
                : "GitHub providers default to OAuth connection."}
            </p>
          ) : null}

          {addMode === "api_key" ? (
            <>
              <label>
                Account ID (optional)
                <input value={addAccountId} onChange={(e) => setAddAccountId(e.target.value)} />
              </label>
              <label>
                Label (optional)
                <input value={addLabel} onChange={(e) => setAddLabel(e.target.value)} />
              </label>
              <label>
                Auth Type
                <input value={addAuthType} onChange={(e) => setAddAuthType(e.target.value)} />
              </label>
              <label>
                API Key
                <input
                  type="password"
                  value={addApiKey}
                  onChange={(e) => setAddApiKey(e.target.value)}
                  placeholder="sk-..."
                />
              </label>
              <button type="button" onClick={() => void props.runAction(submitAdd)}>
                Save Account
              </button>
            </>
          ) : (
            <>
              {addProvider === "codex" ? (
                <p className="note">OAuth connects your ChatGPT Pro/Plus Codex account.</p>
              ) : (
                <>
                  <p className="note">OAuth currently connects GitHub Copilot accounts.</p>
                  <label>
                    Deployment
                    <select value={props.deploymentType} onChange={(e) => props.setDeploymentType(e.target.value)}>
                      <option value="github.com">GitHub.com</option>
                      <option value="enterprise">GitHub Enterprise</option>
                    </select>
                  </label>
                  {props.deploymentType === "enterprise" ? (
                    <label>
                      Enterprise URL/domain
                      <input value={props.enterpriseUrl} onChange={(e) => props.setEnterpriseUrl(e.target.value)} />
                    </label>
                  ) : null}
                </>
              )}
              <div className="row">
                <button type="button" onClick={() => void props.runAction(submitAdd)}>
                  Start OAuth
                </button>
                <button
                  type="button"
                  onClick={() => void props.runAction(completeOauthAndClose)}
                  disabled={!props.deviceFlow}
                >
                  Complete OAuth
                </button>
              </div>
              {props.deviceFlow ? (
                <pre>
{`Visit: ${props.deviceFlow.verification_uri}
Code: ${props.deviceFlow.user_code}
Session: ${props.deviceFlow.session_id}
Provider: ${props.oauthProvider ?? "unknown"}`}
                </pre>
              ) : null}
            </>
          )}
        </Modal>
      ) : null}

      {editTarget ? (
        <Modal title="Account Details" onClose={() => setEditTarget(null)}>
          <div className="detail-form">
            <label>
              Provider
              <input value={editTarget.provider} readOnly />
            </label>
            <label>
              Account ID
              <input value={editTarget.id} readOnly />
            </label>
            <label>
              Label
              <input value={editLabel} onChange={(e) => setEditLabel(e.target.value)} />
            </label>
            <label>
              Auth Type
              <input value={editAuthType} onChange={(e) => setEditAuthType(e.target.value)} />
            </label>
            <label>
              Enabled
              <select
                value={editEnabled ? "enabled" : "disabled"}
                onChange={(e) => setEditEnabled(e.target.value === "enabled")}
              >
                <option value="enabled">Enabled</option>
                <option value="disabled">Disabled</option>
              </select>
            </label>
            <label>
              New API Key (optional)
              <input
                type="password"
                value={editApiKey}
                onChange={(e) => setEditApiKey(e.target.value)}
                placeholder="Leave empty to keep current"
              />
            </label>
          </div>
          {isEditOauthAccount ? (
            <div className="detail-oauth-block">
              <label>
                New oauth access token
                <input
                  type="password"
                  value={editOauthAccessToken}
                  onChange={(e) => setEditOauthAccessToken(e.target.value)}
                  placeholder="Paste new oauth access token"
                />
              </label>
              <button
                type="button"
                className="detail-btn detail-btn-secondary"
                onClick={() => void props.runAction(refreshApiKeyFromOauthToken)}
              >
                Refresh API Key
              </button>
            </div>
          ) : null}
          <button type="button" className="detail-btn detail-btn-primary" onClick={() => void props.runAction(submitEdit)}>
            Save Changes
          </button>
        </Modal>
      ) : null}
    </section>
  );
}

function parseQuotaItems(raw: string | null | undefined): AccountQuotaItem[] {
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .map((row): AccountQuotaItem | null => {
        if (!row || typeof row !== "object") return null;
        const v = row as Record<string, unknown>;
        if (typeof v.name !== "string" || !v.name.trim()) return null;
        return {
          name: v.name,
          total: typeof v.total === "number" ? v.total : null,
          percent: typeof v.percent === "number" ? v.percent : null,
          remaining: typeof v.remaining === "number" ? v.remaining : null,
          expires: typeof v.expires === "string" ? v.expires : null,
        };
      })
      .filter((v): v is AccountQuotaItem => v !== null);
  } catch {
    return [];
  }
}

function formatQuotaNumber(value: number | null | undefined): string {
  if (typeof value !== "number" || Number.isNaN(value)) return "n/a";
  if (Number.isInteger(value)) return String(value);
  return value.toFixed(2);
}

function computeQuotaRemainingPercent(item: AccountQuotaItem): number | null {
  if (typeof item.total === "number" && typeof item.remaining === "number" && item.total > 0) {
    const remaining = (item.remaining / item.total) * 100;
    return Math.max(0, Math.min(100, Number(remaining.toFixed(1))));
  }

  if (typeof item.percent === "number") {
    const remaining = item.percent <= 1 ? item.percent * 100 : item.percent;
    return Math.max(0, Math.min(100, Number(remaining.toFixed(1))));
  }

  return null;
}

import { type ReactNode, useMemo, useState } from "react";

import type { AccountView, RemovedAccountUndo } from "../lib/state";

type AccountsPageProps = {
  providerNames: string[];
  accountsByProvider: Record<string, AccountView[]>;
  removedUndos: RemovedAccountUndo[];
  deploymentType: string;
  setDeploymentType: (value: string) => void;
  enterpriseUrl: string;
  setEnterpriseUrl: (value: string) => void;
  deviceFlow: { verification_uri: string; user_code: string; session_id: string } | null;
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

  const openAdd = () => {
    const provider = props.providerNames[0] ?? "openai";
    const oauthDefault = provider.toLowerCase().includes("github");
    setAddOpen(true);
    setAddProvider(provider);
    setAddMode(oauthDefault ? "oauth" : "api_key");
    setAddAccountId("");
    setAddLabel("");
    setAddAuthType("bearer");
    setAddApiKey("");
  };

  const openAddForProvider = (provider: string) => {
    const oauthDefault = provider.toLowerCase().includes("github");
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
    if (!editOauthAccessToken.trim()) {
      throw new Error("New oauth access token is required");
    }

    await props.onUpdateAccount({
      provider: editTarget.provider,
      accountId: editTarget.id,
      oauthAccessToken: editOauthAccessToken.trim(),
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
      <article className="card card-wide">
        <div className="row row-tight accounts-head">
          <h2>Accounts</h2>
          <button type="button" className="add-account-btn" onClick={openAdd}>
            Add Account
          </button>
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
                <div className="row row-tight provider-head">
                  <h3>{providerName}</h3>
                  <button type="button" className="add-account-btn" onClick={() => openAddForProvider(providerName)}>
                    Add Account
                  </button>
                </div>
                {rows.length === 0 ? (
                  <p className="muted">No accounts</p>
                ) : (
                  <ul>
                    {rows.map((row, index) => {
                      if (row.kind === "undo") {
                        return (
                          <li key={row.undo.key} className="undo-row">
                            <div className="row row-tight">
                              <span>Account removed</span>
                              <button
                                type="button"
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
                      return (
                        <li key={account.id}>
                          <div className="row row-tight">
                            <strong>{account.label}</strong>
                            <span>id: {account.id}</span>
                            <span>{account.auth_type ?? "auth_type: unset"}</span>
                            <span>{account.enabled ? "enabled" : "disabled"}</span>
                            {account.is_default ? <span className="badge">default</span> : null}
                          </div>
                          <div className="row row-tight">
                          <button type="button" onClick={() => openEdit(account)}>
                              Modify
                            </button>
                            <button
                              type="button"
                              onClick={() =>
                                void props.runAction(() => props.onTestAccount(providerName, account.id))
                              }
                            >
                              Test
                            </button>
                            <button
                              type="button"
                              onClick={() =>
                                void props.runAction(() =>
                                  props.onRemoveAccount(providerName, account, Math.max(0, originalIndex))
                                )
                              }
                            >
                              Remove
                            </button>
                          </div>
                          <small>secrets: {account.secret_keys.join(", ") || "none"}</small>
                        </li>
                      );
                    })}
                  </ul>
                )}
              </section>
            );
          })}
        </div>
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
                if (provider.toLowerCase().includes("github")) {
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
              disabled={addProvider.toLowerCase().includes("github")}
            >
              <option value="api_key">API Key</option>
              <option value="oauth">OAuth</option>
            </select>
          </label>
          {addProvider.toLowerCase().includes("github") ? (
            <p className="note">GitHub providers default to OAuth connection.</p>
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
Session: ${props.deviceFlow.session_id}`}
                </pre>
              ) : null}
            </>
          )}
        </Modal>
      ) : null}

      {editTarget ? (
        <Modal title="Modify Account" onClose={() => setEditTarget(null)}>
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
            <select value={editEnabled ? "enabled" : "disabled"} onChange={(e) => setEditEnabled(e.target.value === "enabled")}>
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
          {isEditOauthAccount ? (
            <>
              <label>
                New oauth access token
                <input
                  type="password"
                  value={editOauthAccessToken}
                  onChange={(e) => setEditOauthAccessToken(e.target.value)}
                  placeholder="Paste new oauth access token"
                />
              </label>
              <button type="button" onClick={() => void props.runAction(refreshApiKeyFromOauthToken)}>
                Refresh API Key
              </button>
            </>
          ) : null}
          <button type="button" onClick={() => void props.runAction(submitEdit)}>
            Save Changes
          </button>
        </Modal>
      ) : null}
    </section>
  );
}

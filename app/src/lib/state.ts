export type ProviderStatus = {
  name: string;
  provider_type: string;
  base_url: string;
  enabled: boolean;
  adapter_registered: boolean;
};

export type ModelView = {
  name: string;
  provider: string;
  provider_model: string;
  is_default: boolean;
  enabled: boolean;
};

export type LogLevel = "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR";

export type LogEntry = {
  id: string;
  ts: string;
  level: LogLevel | string;
  target: string;
  message: string;
  request_id?: string | null;
  metadata?: Record<string, string>;
};

export type AccountView = {
  provider: string;
  id: string;
  label: string;
  auth_type?: string;
  is_default: boolean;
  enabled: boolean;
  meta: Record<string, string>;
  secret_keys: string[];
};

export type ConversationMessage = {
  seq: number;
  role: string;
  content_text: string;
  created_at: string;
};

export type ConversationView = {
  id: string;
  created_at: string;
  updated_at: string;
  provider: string;
  account_id?: string | null;
  model: string;
  latest_request_id?: string | null;
  message_count: number;
  preview: string;
  messages: ConversationMessage[];
};

export type CopilotLoginStart = {
  session_id: string;
  verification_uri: string;
  user_code: string;
  expires_in: number;
  interval: number;
  deployment: unknown;
};

export type CopilotComplete = {
  status: string;
  auth_state: unknown | null;
};

export type TabId = "accounts" | "history" | "stream" | "logs" | "config" | "about" | "status";

export type TabSpec = {
  id: TabId;
  label: string;
};

export const ROUTER_BASE_DEFAULT = "http://127.0.0.1:11434";
export const TAB_STORAGE_KEY = "llm-router.active-tab";
export const DEFAULT_TAB: TabId = "accounts";

export const TABS: TabSpec[] = [
  { id: "accounts", label: "Accounts" },
  { id: "history", label: "History" },
  { id: "status", label: "Status" },
  { id: "stream", label: "Stream" },
  { id: "logs", label: "Logs" },
  { id: "config", label: "Config" },
  { id: "about", label: "About" },
];

export function isTabId(value: string): value is TabId {
  return TABS.some((tab) => tab.id === value);
}

export function readStoredTab(): TabId {
  if (typeof window === "undefined") {
    return DEFAULT_TAB;
  }

  const stored = window.localStorage.getItem(TAB_STORAGE_KEY);
  if (!stored || !isTabId(stored)) {
    return DEFAULT_TAB;
  }

  return stored;
}

export function persistTab(tab: TabId): void {
  if (typeof window !== "undefined") {
    window.localStorage.setItem(TAB_STORAGE_KEY, tab);
  }
}

export type AccountsByProvider = Record<string, AccountView[]>;

export function groupAccountsByProvider(accounts: AccountView[]): AccountsByProvider {
  const grouped: AccountsByProvider = {};
  for (const account of accounts) {
    if (!grouped[account.provider]) grouped[account.provider] = [];
    grouped[account.provider].push(account);
  }

  for (const key of Object.keys(grouped)) {
    grouped[key].sort((a, b) => a.label.localeCompare(b.label));
  }

  return grouped;
}

export type CredentialAccountSnapshot = {
  provider: string;
  id: string;
  label: string;
  auth_type?: string;
  is_default: boolean;
  enabled: boolean;
  secrets: Record<string, string>;
  meta: Record<string, string>;
};

export type RemovedAccountUndo = {
  key: string;
  provider: string;
  originalIndex: number;
  snapshot: CredentialAccountSnapshot;
};

export function getCredentialAccountSnapshot(
  config: Record<string, unknown> | null,
  provider: string,
  accountId: string
): CredentialAccountSnapshot | null {
  const providers = (config?.credentials as { providers?: Record<string, unknown> } | undefined)?.providers;
  const providerCfg = providers?.[provider] as { accounts?: unknown[] } | undefined;
  const accounts = providerCfg?.accounts;
  if (!Array.isArray(accounts)) {
    return null;
  }

  const found = accounts.find((entry) => {
    const row = entry as { id?: unknown };
    return typeof row.id === "string" && row.id === accountId;
  }) as
    | {
        id?: string;
        label?: string;
        auth_type?: string;
        is_default?: boolean;
        enabled?: boolean;
        secrets?: Record<string, unknown>;
        meta?: Record<string, unknown>;
      }
    | undefined;

  if (!found || typeof found.id !== "string") {
    return null;
  }

  const secrets: Record<string, string> = {};
  for (const [key, value] of Object.entries(found.secrets ?? {})) {
    if (typeof value === "string") {
      secrets[key] = value;
    }
  }

  const meta: Record<string, string> = {};
  for (const [key, value] of Object.entries(found.meta ?? {})) {
    if (typeof value === "string") {
      meta[key] = value;
    }
  }

  return {
    provider,
    id: found.id,
    label: found.label ?? found.id,
    auth_type: found.auth_type,
    is_default: Boolean(found.is_default),
    enabled: found.enabled !== false,
    secrets,
    meta,
  };
}

import { useEffect, useMemo, useState } from "react";

type ConfigPageProps = {
  config: Record<string, unknown> | null;
  onSetAppConfig: (patch: {
    default_port?: number;
    log_level_filter?: string;
    retention_days?: number;
    request_retention_days?: number;
    https_proxy?: string;
  }) => Promise<void>;
  runAction: (fn: () => Promise<void>) => Promise<void>;
};

type RuntimeAppConfig = {
  default_port?: number;
  log_level_filter?: string;
  retention_days?: number;
  request_retention_days?: number;
  https_proxy?: string;
};

function readRuntimeAppConfig(config: Record<string, unknown> | null): RuntimeAppConfig {
  if (!config) return {};
  const appConfig = config.app_config as RuntimeAppConfig | undefined;
  return appConfig ?? {};
}

function readHttpsProxy(config: Record<string, unknown> | null): string {
  const appConfig = readRuntimeAppConfig(config);
  return typeof appConfig.https_proxy === "string" ? appConfig.https_proxy : "";
}

function readDefaultPort(config: Record<string, unknown> | null): string {
  const appConfig = readRuntimeAppConfig(config);
  return typeof appConfig.default_port === "number" ? String(appConfig.default_port) : "11434";
}

function readLogLevelFilter(config: Record<string, unknown> | null): string {
  const appConfig = readRuntimeAppConfig(config);
  return typeof appConfig.log_level_filter === "string" ? appConfig.log_level_filter : "info";
}

function readRetentionDays(config: Record<string, unknown> | null): string {
  const appConfig = readRuntimeAppConfig(config);
  return typeof appConfig.retention_days === "number" ? String(appConfig.retention_days) : "7";
}

function readRequestRetentionDays(config: Record<string, unknown> | null): string {
  const appConfig = readRuntimeAppConfig(config);
  return typeof appConfig.request_retention_days === "number" ? String(appConfig.request_retention_days) : "30";
}

export function ConfigPage({
  config,
  onSetAppConfig,
  runAction,
}: ConfigPageProps) {
  const currentDefaultPort = useMemo(() => readDefaultPort(config), [config]);
  const currentLogLevelFilter = useMemo(() => readLogLevelFilter(config), [config]);
  const currentRetentionDays = useMemo(() => readRetentionDays(config), [config]);
  const currentRequestRetentionDays = useMemo(() => readRequestRetentionDays(config), [config]);
  const currentHttpsProxy = useMemo(() => readHttpsProxy(config), [config]);

  const [defaultPortInput, setDefaultPortInput] = useState(currentDefaultPort);
  const [logLevelFilterInput, setLogLevelFilterInput] = useState(currentLogLevelFilter);
  const [retentionDaysInput, setRetentionDaysInput] = useState(currentRetentionDays);
  const [requestRetentionDaysInput, setRequestRetentionDaysInput] = useState(currentRequestRetentionDays);
  const [httpsProxyInput, setHttpsProxyInput] = useState(currentHttpsProxy);

  useEffect(() => {
    setDefaultPortInput(currentDefaultPort);
  }, [currentDefaultPort]);

  useEffect(() => {
    setLogLevelFilterInput(currentLogLevelFilter);
  }, [currentLogLevelFilter]);

  useEffect(() => {
    setRetentionDaysInput(currentRetentionDays);
  }, [currentRetentionDays]);

  useEffect(() => {
    setRequestRetentionDaysInput(currentRequestRetentionDays);
  }, [currentRequestRetentionDays]);

  useEffect(() => {
    setHttpsProxyInput(currentHttpsProxy);
  }, [currentHttpsProxy]);

  const onBlurDefaultPort = () => {
    if (defaultPortInput.trim() === currentDefaultPort.trim()) {
      return;
    }
    const parsed = Number.parseInt(defaultPortInput.trim(), 10);
    if (!Number.isFinite(parsed) || parsed < 1 || parsed > 65535) {
      void runAction(async () => {
        throw new Error("default_port must be an integer between 1 and 65535");
      });
      setDefaultPortInput(currentDefaultPort);
      return;
    }
    void runAction(() => onSetAppConfig({ default_port: parsed }));
  };

  const onBlurLogLevelFilter = () => {
    if (logLevelFilterInput.trim() === currentLogLevelFilter.trim()) {
      return;
    }
    if (!logLevelFilterInput.trim()) {
      void runAction(async () => {
        throw new Error("log_level_filter cannot be empty");
      });
      setLogLevelFilterInput(currentLogLevelFilter);
      return;
    }
    void runAction(() => onSetAppConfig({ log_level_filter: logLevelFilterInput }));
  };

  const onBlurRetentionDays = () => {
    if (retentionDaysInput.trim() === currentRetentionDays.trim()) {
      return;
    }
    const parsed = Number.parseInt(retentionDaysInput.trim(), 10);
    if (!Number.isFinite(parsed) || parsed < 1) {
      void runAction(async () => {
        throw new Error("retention_days must be an integer >= 1");
      });
      setRetentionDaysInput(currentRetentionDays);
      return;
    }
    void runAction(() => onSetAppConfig({ retention_days: parsed }));
  };

  const onBlurRequestRetentionDays = () => {
    if (requestRetentionDaysInput.trim() === currentRequestRetentionDays.trim()) {
      return;
    }
    const parsed = Number.parseInt(requestRetentionDaysInput.trim(), 10);
    if (!Number.isFinite(parsed) || parsed < 1) {
      void runAction(async () => {
        throw new Error("request_retention_days must be an integer >= 1");
      });
      setRequestRetentionDaysInput(currentRequestRetentionDays);
      return;
    }
    void runAction(() => onSetAppConfig({ request_retention_days: parsed }));
  };

  const onBlurHttpsProxy = () => {
    if (httpsProxyInput.trim() === currentHttpsProxy.trim()) {
      return;
    }
    void runAction(() => onSetAppConfig({ https_proxy: httpsProxyInput }));
  };

  return (
    <section className="grid">
      <article className="card card-wide">
        <h2>Active Config</h2>
        <div className="row row-tight">
          <label htmlFor="default-port-input">
            <strong>Default Port</strong>
          </label>
          <input
            id="default-port-input"
            type="number"
            min={1}
            max={65535}
            value={defaultPortInput}
            onChange={(event) => setDefaultPortInput(event.target.value)}
            onBlur={onBlurDefaultPort}
          />
        </div>
        <div className="row row-tight">
          <label htmlFor="log-level-filter-input">
            <strong>Log Level Filter</strong>
          </label>
          <input
            id="log-level-filter-input"
            type="text"
            placeholder="info"
            value={logLevelFilterInput}
            onChange={(event) => setLogLevelFilterInput(event.target.value)}
            onBlur={onBlurLogLevelFilter}
          />
        </div>
        <div className="row row-tight">
          <label htmlFor="retention-days-input">
            <strong>Retention Days</strong>
          </label>
          <input
            id="retention-days-input"
            type="number"
            min={1}
            value={retentionDaysInput}
            onChange={(event) => setRetentionDaysInput(event.target.value)}
            onBlur={onBlurRetentionDays}
          />
        </div>
        <div className="row row-tight">
          <label htmlFor="request-retention-days-input">
            <strong>Request Retention Days</strong>
          </label>
          <input
            id="request-retention-days-input"
            type="number"
            min={1}
            value={requestRetentionDaysInput}
            onChange={(event) => setRequestRetentionDaysInput(event.target.value)}
            onBlur={onBlurRequestRetentionDays}
          />
        </div>
        <div className="row row-tight">
          <label htmlFor="https-proxy-input">
            <strong>HTTPS Proxy</strong>
          </label>
          <input
            id="https-proxy-input"
            type="text"
            placeholder="http://127.0.0.1:7890"
            value={httpsProxyInput}
            onChange={(event) => setHttpsProxyInput(event.target.value)}
            onBlur={onBlurHttpsProxy}
          />
        </div>
        <p className="muted">Applies on blur and writes to config.yaml. Restart app for full effect.</p>
        <pre>{JSON.stringify(config, null, 2)}</pre>
      </article>
    </section>
  );
}

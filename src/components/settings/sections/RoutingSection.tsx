import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { useQueries, useQueryClient } from "@tanstack/react-query";
import { toast } from "@/lib/toast";
import {
  ChevronRight,
  Loader2,
  Lock,
  Globe,
  ShieldCheck,
  ShieldAlert,
  Copy,
  Check,
  ListFilter,
  X,
} from "lucide-react";
import type { AppId } from "@/lib/api";
import { proxyApi } from "@/lib/api/proxy";
import { useProvidersQuery } from "@/lib/query";
import type { HostOption } from "@/types/proxy";
import {
  useGlobalProxyConfig,
  useProxyClientHost,
  useProxyEndpointsInfo,
  useProxyStatusQuery,
  useSetProxyClientHost,
  useUpdateGlobalProxyConfig,
} from "@/lib/query/proxy";
import { useProxyStatus } from "@/hooks/useProxyStatus";
import {
  MODE_APP_IDS,
  PROXY_APP_IDS,
  type ProxyAppId,
} from "@/config/appConfig";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { HelpTip, DisabledReason } from "@/components/ui/help-tip";
import { SegmentedControl } from "@/components/ui/segmented-control";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { AutoFailoverConfigPanel } from "@/components/proxy/AutoFailoverConfigPanel";
import { RectifierConfigPanel } from "@/components/settings/RectifierConfigPanel";
import { APP_DISPLAY_NAME, AppGlyph } from "@/components/shell/AppGlyph";
import { MODE_TONE } from "@/components/providers/mode/ModeTabs";
import {
  SettingsBlock,
  SettingsCard,
  SettingsRow,
  SettingsSwitchRow,
} from "@/components/settings/SettingsLayout";
import type { AppMode } from "@/types/proxy";
import { cn } from "@/lib/utils";

interface RoutingSectionProps {
  /** 「前往」打开这个应用的供应商页 */
  onOpenApp: (app: AppId) => void;
}

const isValidListenAddress = (address: string): boolean => {
  if (address === "localhost" || address === "0.0.0.0") return true;
  if (/^(\d{1,3}\.){3}\d{1,3}$/.test(address)) {
    return address.split(".").every((part) => Number(part) <= 255);
  }
  // 域名支持（如 my-node.ts.net 或局域网主机名）
  if (
    /^(?=.{1,253}$)[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*$/i.test(
      address,
    )
  ) {
    return true;
  }
  // IPv6 字面量：必须含 `:` 且能在 [..] 包装后被 URL 解析器接受（后端会把 `::` 改写成 `::1`）
  if (!address.includes(":")) return false;
  try {
    new URL(`http://[${address}]/`);
    return true;
  } catch {
    return false;
  }
};

const isLoopbackHost = (host: string): boolean => {
  const h = host.trim().replace(/\.$/, "");
  if (
    h.toLowerCase() === "localhost" ||
    h.toLowerCase().endsWith(".localhost")
  ) {
    return true;
  }
  if (h === "::1" || h === "::" || h === "[::1]" || h === "[::]") return true;
  if (/^127\.\d+\.\d+\.\d+$/.test(h) || h === "0.0.0.0") return true;
  return false;
};

/** 客户端地址：空（按监听地址）、IP 或主机名；`0.0.0.0` / `::` 客户端连不上 */
const isValidClientHost = (host: string): boolean => {
  if (host === "") return true;
  if (host === "0.0.0.0" || host === "::") return false;
  if (/^[\d.]+$/.test(host) || host.includes(":")) {
    return isValidListenAddress(host);
  }
  return /^(?=.{1,253}$)[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*$/i.test(
    host,
  );
};

/** 没设客户端地址时写给客户端的主机，和后端 `proxy_origin` 一致 */
const defaultClientHost = (listenAddress: string): string =>
  listenAddress === "0.0.0.0"
    ? "127.0.0.1"
    : listenAddress === "::"
      ? "::1"
      : listenAddress;

function formatUptime(seconds: number, t: TFunction) {
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  return hours > 0
    ? t("routingSettings.uptimeHours", { hours, minutes })
    : t("routingSettings.uptimeMinutes", { minutes: Math.max(minutes, 0) });
}

function getHostOptionLabel(host: HostOption, t: TFunction): string {
  switch (host.kind) {
    case "loopback":
      return t(
        "routingSettings.hostOptions.loopback",
        "127.0.0.1 (本地回环 - 明文 HTTP)",
      );
    case "tailscaleDomain":
      return t(
        "routingSettings.hostOptions.tailscaleDomain",
        `${host.value} (Tailscale 域名 - 开启 HTTPS)`,
        { host: host.value },
      );
    case "tailscaleIp":
      return t(
        "routingSettings.hostOptions.tailscaleIp",
        `${host.value} (Tailscale IP - 开启 HTTPS)`,
        { ip: host.value },
      );
    case "all":
      return t("routingSettings.hostOptions.all", "0.0.0.0 (全部网卡)");
    case "customPreset":
      return t(
        "routingSettings.hostOptions.customPreset",
        `${host.value} (自定义预设)`,
        { address: host.value },
      );
    case "custom":
      return t(
        "routingSettings.hostOptions.custom",
        `${host.value} (当前自定义配置)`,
        { address: host.value },
      );
    default:
      return host.label || host.value;
  }
}

/**
 * 设置 → 本地路由（v7）：服务（状态、统计、启停、监听地址、请求日志）→ 正在使用路由的应用
 * → 全部回到直连 → 故障转移参数 → 整流与优化。进出路由在各应用页的模式 tab 里。
 */
export function RoutingSection({ onOpenApp }: RoutingSectionProps) {
  const { t } = useTranslation();
  const { data: status } = useProxyStatusQuery();
  const running = status?.running ?? false;
  const { startProxyServer, stopProxyServer, stopWithRestore, isPending } =
    useProxyStatus();
  const { data: config } = useGlobalProxyConfig();
  const updateConfig = useUpdateGlobalProxyConfig();
  const { data: savedClientHost } = useProxyClientHost();
  const saveClientHostMutation = useSetProxyClientHost();
  const { data: endpointsInfo } = useProxyEndpointsInfo();
  const [address, setAddress] = useState("127.0.0.1");
  const [port, setPort] = useState("15721");
  const [addressError, setAddressError] = useState<string | null>(null);
  const [portError, setPortError] = useState<string | null>(null);
  const [isCustomAddress, setIsCustomAddress] = useState(false);
  const [clientHost, setClientHost] = useState("");
  const [clientHostError, setClientHostError] = useState<string | null>(null);
  const [confirmExitAll, setConfirmExitAll] = useState(false);
  const [failoverApp, setFailoverApp] = useState<ProxyAppId>("claude");
  const [copiedGateway, setCopiedGateway] = useState(false);
  const queryClient = useQueryClient();

  const fallbackHosts: HostOption[] = [
    {
      value: "127.0.0.1",
      kind: "loopback",
      label: "127.0.0.1 (Loopback)",
      description: "Local loopback",
      isTls: false,
    },
    {
      value: "0.0.0.0",
      kind: "all",
      label: "0.0.0.0 (All)",
      description: "All interfaces",
      isTls: false,
    },
  ];

  const availableHosts = endpointsInfo?.availableHosts ?? fallbackHosts;

  const handleCopyGatewayUrl = async (url: string) => {
    try {
      await navigator.clipboard.writeText(url);
      setCopiedGateway(true);
      toast.success(t("routingSettings.copied", "已复制到剪贴板"));
      setTimeout(() => setCopiedGateway(false), 2000);
    } catch {
      // ignore
    }
  };

  const handleDeletePreset = async (hostToDelete: string) => {
    try {
      await proxyApi.deleteCustomListenHost(hostToDelete);
      await queryClient.invalidateQueries({ queryKey: ["proxy", "endpoints"] });
      toast.success(t("routingSettings.presetDeleted", "已删除预设"));
      if (address === hostToDelete && config?.listenAddress !== hostToDelete) {
        setAddress("127.0.0.1");
      }
    } catch (err) {
      toast.error(String(err));
    }
  };

  useEffect(() => {
    if (config) {
      setAddress(config.listenAddress);
      setPort(String(config.listenPort));
    }
  }, [config]);

  useEffect(() => {
    if (config && availableHosts.length > 0) {
      const isPreset = availableHosts.some(
        (h) => h.value === config.listenAddress,
      );
      setIsCustomAddress(!isPreset);
    }
  }, [config, availableHosts]);

  useEffect(() => {
    if (savedClientHost !== undefined) setClientHost(savedClientHost);
  }, [savedClientHost]);

  const liveTargetHost = clientHost.trim() || address.trim() || "127.0.0.1";
  const liveIsTls = !isLoopbackHost(liveTargetHost);
  const liveDisplayHost =
    liveTargetHost === "0.0.0.0" ? "127.0.0.1" : liveTargetHost;
  const livePort = port.trim() || "15721";
  const liveEffectiveOrigin = `${liveIsTls ? "https" : "http"}://${
    liveDisplayHost.includes(":") && !liveDisplayHost.startsWith("[")
      ? `[${liveDisplayHost}]`
      : liveDisplayHost
  }:${livePort}`;

  const modeQueries = useQueries({
    queries: MODE_APP_IDS.map((app) => ({
      queryKey: ["providers", app, "mode"],
      queryFn: () => proxyApi.getAppMode(app),
    })),
  });
  const providerQueries = {
    claude: useProvidersQuery("claude").data,
    codex: useProvidersQuery("codex").data,
    gemini: useProvidersQuery("gemini").data,
    grokbuild: useProvidersQuery("grokbuild").data,
    "claude-desktop": useProvidersQuery("claude-desktop").data,
  };

  const usingApps = MODE_APP_IDS.flatMap((app, index) => {
    const view = modeQueries[index]?.data;
    if (!view || view.mode === "direct") return [];
    const routeName =
      (view.routeProviderId &&
        providerQueries[app]?.providers[view.routeProviderId]?.name) ||
      "—";
    return [{ app, mode: view.mode as AppMode, routeName }];
  });
  const usingCount = usingApps.length;

  const saveListen = async () => {
    if (!config) return;
    const trimmed = address.trim();
    const portValue = Number(port.trim());
    const addressOk = isValidListenAddress(trimmed);
    const portOk =
      /^\d+$/.test(port.trim()) && portValue >= 1024 && portValue <= 65535;
    setAddressError(addressOk ? null : t("proxy.settings.invalidAddress"));
    setPortError(portOk ? null : t("proxy.settings.invalidPort"));
    if (!addressOk || !portOk) return;
    try {
      await updateConfig.mutateAsync({
        ...config,
        listenAddress: trimmed === "localhost" ? "127.0.0.1" : trimmed,
        listenPort: portValue,
      });
    } catch {
      // useUpdateGlobalProxyConfig 的 onSuccess / onError 已经弹过 toast
    }
  };

  const listenDirty =
    !!config &&
    (address.trim() !== config.listenAddress ||
      port.trim() !== String(config.listenPort));

  const saveClientHost = async () => {
    const trimmed = clientHost.trim();
    const ok = isValidClientHost(trimmed);
    setClientHostError(ok ? null : t("routingSettings.clientHostInvalid"));
    if (!ok) return;
    try {
      await saveClientHostMutation.mutateAsync(trimmed);
    } catch {
      // useSetProxyClientHost 的 onSuccess / onError 已经弹过 toast
    }
  };

  const toggleLogging = async (enabled: boolean) => {
    if (!config) return;
    try {
      await updateConfig.mutateAsync({ ...config, enableLogging: enabled });
    } catch {
      toast.error(t("proxy.logging.failed"));
    }
  };

  const stopReason =
    running && usingCount > 0 ? t("routingSettings.stopBlocked") : undefined;

  return (
    <>
      <SettingsBlock title={t("routingSettings.service")}>
        <SettingsCard>
          <div className="flex flex-wrap items-center gap-x-6 gap-y-3 px-4 py-3.5">
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1 text-body font-medium text-fg-1">
                {t("routingSettings.serviceName")}
                <HelpTip title={t("routingSettings.serviceName")}>
                  {t("routingSettings.serviceHelp")}
                </HelpTip>
              </div>
              <div className="mt-0.5 flex items-center gap-1.5 text-caption text-fg-2">
                <span
                  aria-hidden="true"
                  className={cn(
                    "h-1.5 w-1.5 rounded-full",
                    running ? "bg-route" : "bg-fg-3",
                  )}
                />
                {running
                  ? t("routingSettings.runningFor", {
                      uptime: formatUptime(status?.uptime_seconds ?? 0, t),
                    })
                  : t("routingSettings.stopped")}
              </div>
            </div>
            {running && (
              <dl className="flex gap-6 text-end">
                {[
                  [
                    t("routingSettings.totalRequests"),
                    status?.total_requests ?? 0,
                  ],
                  [
                    t("routingSettings.successRate"),
                    `${(status?.success_rate ?? 0).toFixed(1)}%`,
                  ],
                  [
                    t("routingSettings.activeConnections"),
                    status?.active_connections ?? 0,
                  ],
                ].map(([label, value]) => (
                  <div key={String(label)}>
                    <dt className="text-caption text-fg-2">{label}</dt>
                    <dd className="m-0 text-strong font-semibold tabular-nums text-fg-1">
                      {typeof value === "number"
                        ? value.toLocaleString()
                        : value}
                    </dd>
                  </div>
                ))}
              </dl>
            )}
            {running ? (
              <DisabledReason reason={stopReason} align="end">
                <Button
                  variant="neutral"
                  size="regular"
                  disabled={isPending}
                  onClick={() => void stopProxyServer()}
                >
                  {t("routingSettings.stop")}
                </Button>
              </DisabledReason>
            ) : (
              <Button
                variant="neutral"
                size="regular"
                disabled={isPending}
                onClick={() => void startProxyServer()}
              >
                {t("routingSettings.start")}
              </Button>
            )}
          </div>

          <SettingsRow
            label={
              <span className="whitespace-nowrap font-medium text-fg-1">
                {t("routingSettings.listen")}
              </span>
            }
            control={
              <div className="flex items-start gap-2.5">
                <div className="space-y-1.5">
                  <span className="block text-caption font-medium text-fg-2">
                    {t("routingSettings.address")}
                  </span>
                  {isCustomAddress ? (
                    <div className="flex items-center gap-1.5 w-[260px]">
                      <Input
                        value={address}
                        placeholder={t(
                          "routingSettings.customAddressPlaceholder",
                          "例如 192.168.1.100 或 node.example.ts.net",
                        )}
                        onChange={(event) => setAddress(event.target.value)}
                        aria-invalid={addressError ? true : undefined}
                        className="w-full h-9 font-mono text-xs rounded-lg"
                        autoFocus
                      />
                      <Button
                        variant="outline"
                        size="compact"
                        type="button"
                        onClick={() => {
                          setIsCustomAddress(false);
                          setAddress("127.0.0.1");
                          setAddressError(null);
                        }}
                        title={t(
                          "routingSettings.backToPresets",
                          "切换回预设列表",
                        )}
                        className="h-9 px-2 text-xs text-fg-2 hover:text-fg-1 rounded-lg shrink-0 border border-border/80 hover:bg-subtle"
                      >
                        <ListFilter className="h-3.5 w-3.5 mr-1" />
                        {t("routingSettings.presets", "预设")}
                      </Button>
                    </div>
                  ) : (
                    <Select
                      value={
                        availableHosts.some((h) => h.value === address)
                          ? address
                          : "custom"
                      }
                      onValueChange={(val) => {
                        if (val !== "custom") {
                          setAddress(val);
                          setIsCustomAddress(false);
                          setAddressError(null);
                        } else {
                          setIsCustomAddress(true);
                        }
                      }}
                    >
                      <SelectTrigger className="w-[260px] h-9 text-xs rounded-lg">
                        <SelectValue
                          placeholder={t("routingSettings.address")}
                        />
                      </SelectTrigger>
                      <SelectContent className="max-h-72">
                        {availableHosts.map((host) => (
                          <SelectItem key={host.value} value={host.value}>
                            <div className="flex items-center justify-between gap-2.5 w-full pr-1">
                              <span className="font-mono text-xs text-fg-1 truncate">
                                {getHostOptionLabel(host, t)}
                              </span>
                              <div className="flex items-center gap-1.5 shrink-0">
                                {host.isTls ? (
                                  <span className="inline-flex items-center gap-1 rounded-md bg-emerald-500/10 px-1.5 py-0.5 text-[10px] font-medium text-emerald-600 dark:text-emerald-400 border border-emerald-500/20 shrink-0">
                                    <Lock className="h-2.5 w-2.5" />
                                    HTTPS
                                  </span>
                                ) : null}
                                {host.kind === "customPreset" && (
                                  <button
                                    type="button"
                                    onPointerDown={(e) => {
                                      e.stopPropagation();
                                    }}
                                    onClick={(e) => {
                                      e.stopPropagation();
                                      e.preventDefault();
                                      void handleDeletePreset(host.value);
                                    }}
                                    title={t(
                                      "routingSettings.deletePreset",
                                      "删除此预设",
                                    )}
                                    className="inline-flex items-center justify-center h-4 w-4 rounded text-fg-3 hover:text-danger-text hover:bg-danger/10 transition-colors cursor-pointer"
                                  >
                                    <X className="h-3 w-3" />
                                  </button>
                                )}
                              </div>
                            </div>
                          </SelectItem>
                        ))}
                        <SelectSeparator />
                        <SelectItem value="custom">
                          <span className="text-xs text-fg-2">
                            {t("routingSettings.customOption", "自定义输入...")}
                          </span>
                        </SelectItem>
                      </SelectContent>
                    </Select>
                  )}
                  {addressError && (
                    <span className="block text-caption text-danger-text">
                      {addressError}
                    </span>
                  )}
                </div>

                <label className="space-y-1.5">
                  <span className="block text-caption font-medium text-fg-2">
                    {t("routingSettings.port")}
                  </span>
                  <Input
                    value={port}
                    inputMode="numeric"
                    onChange={(event) => setPort(event.target.value)}
                    aria-invalid={portError ? true : undefined}
                    className="w-[84px] h-9 font-mono text-xs rounded-lg"
                  />
                  <span
                    className={cn(
                      "block text-caption",
                      portError ? "text-danger-text" : "text-fg-3",
                    )}
                  >
                    {portError ?? "1024–65535"}
                  </span>
                </label>

                <Button
                  variant="neutral"
                  size="regular"
                  className="mt-[24px] h-9 rounded-lg px-3.5 text-xs whitespace-nowrap"
                  disabled={!listenDirty || updateConfig.isPending}
                  onClick={() => void saveListen()}
                >
                  {updateConfig.isPending && (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  )}
                  {t("routingSettings.saveAndRestart")}
                </Button>
              </div>
            }
          >
            {/* 实时证书与网关状态条 */}
            <div className="rounded-xl border border-border/80 bg-subtle/40 dark:bg-subtle/20 p-3.5 shadow-2xs">
              <div className="flex flex-wrap items-center justify-between gap-y-2.5 gap-x-4">
                <div className="flex flex-wrap items-center gap-2.5 text-xs">
                  {liveIsTls ? (
                    <span className="inline-flex items-center gap-1.5 rounded-full bg-emerald-500/15 px-2.5 py-0.5 text-xs font-medium text-emerald-700 dark:text-emerald-300 border border-emerald-500/30">
                      <Lock className="h-3.5 w-3.5 shrink-0" />
                      {t("routingSettings.httpsMode", "HTTPS 终结模式")}
                    </span>
                  ) : (
                    <span className="inline-flex items-center gap-1.5 rounded-full bg-muted px-2.5 py-0.5 text-xs font-medium text-fg-2 border border-border/80">
                      <Globe className="h-3.5 w-3.5 shrink-0" />
                      {t("routingSettings.httpMode", "HTTP 明文模式")}
                    </span>
                  )}
                  <span className="text-fg-2 flex items-center gap-1.5">
                    {liveIsTls ? (
                      endpointsInfo?.certAvailable ? (
                        <span className="inline-flex items-center gap-1.5 text-fg-2">
                          <ShieldCheck className="h-4 w-4 text-emerald-500 shrink-0" />
                          <span>
                            {t(
                              "routingSettings.certDetected",
                              "已检测到证书：",
                            )}
                          </span>
                          <code className="rounded-md bg-card dark:bg-muted/60 px-1.5 py-0.5 font-mono text-xs font-medium text-fg-1 border border-border/80">
                            {endpointsInfo.detectedCertFile ??
                              t(
                                "routingSettings.defaultCertName",
                                "Tailscale 证书",
                              )}
                          </code>
                        </span>
                      ) : (
                        <span className="inline-flex items-center gap-1.5 text-danger-text font-medium">
                          <ShieldAlert className="h-4 w-4 shrink-0 text-danger-text" />
                          {t(
                            "routingSettings.noCertWarning",
                            "未检测到有效证书（需在 .cc-switch/tls 目录下放置）",
                          )}
                        </span>
                      )
                    ) : (
                      <span className="text-fg-3">
                        {t(
                          "routingSettings.loopbackNotice",
                          "本地回环通信，无加密开销",
                        )}
                      </span>
                    )}
                  </span>
                </div>
                <div className="flex items-center gap-2 text-xs">
                  <span className="text-fg-3 font-medium">
                    {t(
                      "routingSettings.clientAddressPreview",
                      "写入客户端地址：",
                    )}
                  </span>
                  <button
                    type="button"
                    onClick={() =>
                      void handleCopyGatewayUrl(liveEffectiveOrigin)
                    }
                    title={t("routingSettings.copyAddress", "复制网关地址")}
                    className="group inline-flex items-center gap-1.5 rounded-lg border border-border/80 bg-background dark:bg-card px-2.5 py-1 font-mono text-xs text-fg-1 shadow-2xs hover:border-accent hover:bg-subtle transition-all cursor-pointer"
                  >
                    <span>{liveEffectiveOrigin}</span>
                    {copiedGateway ? (
                      <Check className="h-3.5 w-3.5 text-emerald-500 shrink-0" />
                    ) : (
                      <Copy className="h-3.5 w-3.5 text-fg-3 group-hover:text-fg-1 shrink-0 transition-colors" />
                    )}
                  </button>
                </div>
              </div>
            </div>
          </SettingsRow>
          <SettingsRow
            label={t("routingSettings.clientHost")}
            help={{
              title: t("routingSettings.clientHost"),
              body: t("routingSettings.clientHostHelp"),
            }}
            control={
              <div className="flex items-start gap-3">
                <label className="space-y-1">
                  <Input
                    value={clientHost}
                    placeholder={defaultClientHost(
                      config?.listenAddress ?? "127.0.0.1",
                    )}
                    onChange={(event) => setClientHost(event.target.value)}
                    aria-label={t("routingSettings.clientHost")}
                    aria-invalid={clientHostError ? true : undefined}
                    className="w-[262px]"
                  />
                  <span
                    className={cn(
                      "block text-caption",
                      clientHostError ? "text-danger-text" : "text-fg-3",
                    )}
                  >
                    {clientHostError ?? t("routingSettings.clientHostHint")}
                  </span>
                </label>
                <Button
                  variant="neutral"
                  size="regular"
                  disabled={
                    savedClientHost === undefined ||
                    saveClientHostMutation.isPending
                  }
                  onClick={() => void saveClientHost()}
                >
                  {t("common.save")}
                </Button>
              </div>
            }
          />
          <SettingsSwitchRow
            label={t("routingSettings.logging")}
            help={{
              title: t("routingSettings.logging"),
              body: t("routingSettings.loggingHelp"),
            }}
            checked={config?.enableLogging ?? false}
            disabled={!config}
            onCheckedChange={(value) => void toggleLogging(value)}
          />
        </SettingsCard>
      </SettingsBlock>

      <SettingsBlock
        title={t("routingSettings.usingApps", { count: usingCount })}
      >
        {usingCount === 0 ? (
          <div className="rounded-panel border border-dashed border-border px-4 py-3 text-caption text-fg-3">
            {t("routingSettings.noApps")}
          </div>
        ) : (
          <SettingsCard>
            {usingApps.map(({ app, mode, routeName }) => (
              <UsingAppRow
                key={app}
                app={app}
                modeLabel={t(`mode.names.${mode}`)}
                dotClass={MODE_TONE[mode].dot}
                detail={
                  mode === "stack"
                    ? t("routingSettings.stackDetail", { name: routeName })
                    : t("routingSettings.routeDetail", { name: routeName })
                }
                onOpen={() => onOpenApp(app)}
              />
            ))}
          </SettingsCard>
        )}
        {usingApps.length > 0 && (
          <Button
            variant="neutral"
            size="regular"
            onClick={() => setConfirmExitAll(true)}
          >
            {t("routingSettings.exitAll")}
          </Button>
        )}
      </SettingsBlock>

      <SettingsBlock
        title={t("settings.advanced.failover.title")}
        help={{
          title: t("settings.advanced.failover.title"),
          body: t("routingSettings.failoverHelp"),
        }}
      >
        <div className="space-y-4 rounded-panel border border-border bg-surface p-5">
          <SegmentedControl
            size="sm"
            aria-label={t("settings.advanced.failover.title")}
            value={failoverApp}
            onValueChange={setFailoverApp}
            items={PROXY_APP_IDS.map((app) => ({
              value: app,
              label: APP_DISPLAY_NAME[app],
            }))}
          />
          <AutoFailoverConfigPanel key={failoverApp} appType={failoverApp} />
        </div>
      </SettingsBlock>

      <SettingsBlock
        title={t("settings.advanced.rectifier.title")}
        help={{
          title: t("settings.advanced.rectifier.title"),
          body: t("settings.advanced.rectifier.description"),
        }}
      >
        <div className="rounded-panel border border-border bg-surface p-5">
          <RectifierConfigPanel />
        </div>
      </SettingsBlock>

      {/* 可恢复的操作（路由目标、队列、聚合名单都保留），不用红色 */}
      <ConfirmDialog
        isOpen={confirmExitAll}
        variant="info"
        title={t("routingSettings.exitAllTitle")}
        message={t("routingSettings.exitAllMessage")}
        confirmText={t("routingSettings.exitAllConfirm")}
        onConfirm={() => {
          setConfirmExitAll(false);
          void stopWithRestore();
        }}
        onCancel={() => setConfirmExitAll(false)}
      />
    </>
  );
}

function UsingAppRow({
  app,
  modeLabel,
  dotClass,
  detail,
  onOpen,
}: {
  app: AppId;
  modeLabel: string;
  dotClass: string;
  detail: string;
  onOpen: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center gap-3 px-4 py-2.5">
      <AppGlyph app={app} size={18} badgeClassName="bg-surface" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1.5 text-body font-medium text-fg-1">
          {APP_DISPLAY_NAME[app]}
          <span className="text-fg-3">·</span>
          <span
            aria-hidden="true"
            className={cn("h-1.5 w-1.5 rounded-full", dotClass)}
          />
          <span className="font-normal text-fg-2">{modeLabel}</span>
        </div>
        <div className="truncate text-caption text-fg-2">{detail}</div>
      </div>
      <Button variant="quiet" size="compact" onClick={onOpen}>
        {t("routingSettings.goTo")}
        <ChevronRight className="h-3.5 w-3.5" />
      </Button>
    </div>
  );
}

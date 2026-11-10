import { QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { RoutingSection } from "@/components/settings/sections/RoutingSection";
import { proxyKeys } from "@/lib/query/proxy";
import type {
  GlobalProxyConfig,
  ProxyEndpointsInfo,
  ProxyStatus,
} from "@/types/proxy";
import { createTestQueryClient } from "../utils/testQueryClient";

if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = () => {};
}

const proxyApiMock = vi.hoisted(() => ({
  getProxyStatus: vi.fn(),
  getProxyTakeoverStatus: vi.fn(),
  getGlobalProxyConfig: vi.fn(),
  getProxyClientHost: vi.fn(),
  getProxyEndpointsInfo: vi.fn(),
  getAppMode: vi.fn(),
  setProxyClientHost: vi.fn(),
  deleteCustomListenHost: vi.fn(),
  updateGlobalProxyConfig: vi.fn(),
  startProxyServer: vi.fn(),
  stopProxyServer: vi.fn(),
  stopProxyWithRestore: vi.fn(),
}));

vi.mock("@/lib/api/proxy", () => ({
  proxyApi: proxyApiMock,
}));

vi.mock("@/lib/query", () => ({
  useProvidersQuery: () => ({ data: { providers: {} } }),
}));

vi.mock("@/components/proxy/AutoFailoverConfigPanel", () => ({
  AutoFailoverConfigPanel: () => <div data-testid="failover-panel" />,
}));

vi.mock("@/components/settings/RectifierConfigPanel", () => ({
  RectifierConfigPanel: () => <div data-testid="rectifier-panel" />,
}));

const baseConfig: GlobalProxyConfig = {
  proxyEnabled: true,
  listenAddress: "127.0.0.1",
  listenPort: 15721,
  enableLogging: false,
};

const baseStatus: ProxyStatus = {
  running: false,
  address: "127.0.0.1",
  port: 15721,
  active_connections: 0,
  total_requests: 0,
  success_requests: 0,
  failed_requests: 0,
  success_rate: 0,
  uptime_seconds: 0,
  current_provider: null,
  current_provider_id: null,
  last_request_at: null,
  last_error: null,
  failover_count: 0,
};

const baseEndpoints: ProxyEndpointsInfo = {
  listenAddress: "127.0.0.1",
  listenPort: 15721,
  clientHost: "",
  effectiveOrigin: "http://127.0.0.1:15721",
  isTls: false,
  certAvailable: false,
  availableHosts: [
    {
      value: "127.0.0.1",
      kind: "loopback",
      label: "127.0.0.1",
      description: "Loopback",
      isTls: false,
    },
    {
      value: "0.0.0.0",
      kind: "all",
      label: "0.0.0.0",
      description: "All",
      isTls: false,
    },
    {
      value: "::",
      kind: "customPreset",
      label: "::",
      description: "IPv6 all",
      isTls: false,
    },
    {
      value: "100.64.0.1",
      kind: "tailscaleIp",
      label: "100.64.0.1",
      description: "Tailscale IP",
      isTls: true,
    },
  ],
};

function renderRouting(overrides?: {
  config?: Partial<GlobalProxyConfig>;
  clientHost?: string;
  endpoints?: Partial<ProxyEndpointsInfo>;
}) {
  proxyApiMock.getGlobalProxyConfig.mockResolvedValue({
    ...baseConfig,
    ...overrides?.config,
  });
  proxyApiMock.getProxyClientHost.mockResolvedValue(
    overrides?.clientHost ?? "",
  );
  proxyApiMock.getProxyEndpointsInfo.mockResolvedValue({
    ...baseEndpoints,
    ...overrides?.endpoints,
  });

  const queryClient = createTestQueryClient();
  const invalidateSpy = vi.spyOn(queryClient, "invalidateQueries");
  const utils = render(
    <QueryClientProvider client={queryClient}>
      <RoutingSection onOpenApp={() => {}} />
    </QueryClientProvider>,
  );

  return { ...utils, queryClient, invalidateSpy };
}

describe("RoutingSection", () => {
  beforeEach(() => {
    Object.values(proxyApiMock).forEach((fn) => fn.mockReset());
    proxyApiMock.getProxyStatus.mockResolvedValue(baseStatus);
    proxyApiMock.getProxyTakeoverStatus.mockResolvedValue({
      claude: false,
      codex: false,
      gemini: false,
      grokbuild: false,
      opencode: false,
      openclaw: false,
      hermes: false,
    });
    proxyApiMock.getAppMode.mockResolvedValue({
      mode: "direct",
      attached: false,
      routeProviderId: null,
      directProviderId: null,
    });
    proxyApiMock.setProxyClientHost.mockResolvedValue(undefined);
    proxyApiMock.deleteCustomListenHost.mockResolvedValue(undefined);
  });

  it("shows HTTPS termination mode for wildcard listener with non-loopback clientHost", async () => {
    renderRouting({
      config: { listenAddress: "0.0.0.0" },
      clientHost: "100.64.0.1",
      endpoints: { certAvailable: true, detectedCertFile: "node.ts.net.crt" },
    });

    await waitFor(() => {
      expect(screen.getByText("HTTPS 终结模式")).toBeInTheDocument();
      expect(screen.getByText("https://100.64.0.1:15721")).toBeInTheDocument();
    });
  });

  it("keeps HTTP mode for strict loopback listener even with HTTPS clientHost preview", async () => {
    renderRouting({
      config: { listenAddress: "127.0.0.1" },
      clientHost: "my-node.ts.net",
    });

    await waitFor(() => {
      expect(screen.getByText("HTTP 明文模式")).toBeInTheDocument();
      expect(
        screen.getByText("https://my-node.ts.net:15721"),
      ).toBeInTheDocument();
    });
  });

  it("normalizes IPv6 wildcard :: preview to [::1] without forcing HTTPS", async () => {
    renderRouting({
      config: { listenAddress: "::" },
      clientHost: "",
    });

    await waitFor(() => {
      expect(screen.getByText("HTTP 明文模式")).toBeInTheDocument();
      expect(screen.getByText("http://[::1]:15721")).toBeInTheDocument();
    });
  });

  it("rejects unspecified clientHost values 0.0.0.0 and ::", async () => {
    renderRouting();

    const input = await screen.findByRole("textbox", {
      name: "routingSettings.clientHost",
    });
    const saveButtons = screen.getAllByRole("button", { name: "common.save" });
    const clientHostSave = saveButtons[saveButtons.length - 1];

    await waitFor(() => expect(clientHostSave).not.toBeDisabled());

    for (const invalid of ["0.0.0.0", "::", "[::]"]) {
      fireEvent.change(input, { target: { value: invalid } });
      fireEvent.click(clientHostSave);
      await waitFor(() => {
        expect(
          screen.getByText("routingSettings.clientHostInvalid"),
        ).toBeInTheDocument();
      });
    }

    expect(proxyApiMock.setProxyClientHost).not.toHaveBeenCalled();
  });

  it("invalidates clientHost, endpointsInfo, and status when setProxyClientHost fails", async () => {
    proxyApiMock.setProxyClientHost.mockRejectedValue(
      new Error("TLS cert missing"),
    );
    const { invalidateSpy } = renderRouting({
      config: { listenAddress: "0.0.0.0" },
    });

    const input = await screen.findByRole("textbox", {
      name: "routingSettings.clientHost",
    });
    const saveButtons = screen.getAllByRole("button", { name: "common.save" });
    const clientHostSave = saveButtons[saveButtons.length - 1];
    await waitFor(() => expect(clientHostSave).not.toBeDisabled());

    fireEvent.change(input, { target: { value: "100.64.0.1" } });
    fireEvent.click(clientHostSave);

    await waitFor(() => {
      expect(proxyApiMock.setProxyClientHost).toHaveBeenCalledWith(
        "100.64.0.1",
      );
      expect(invalidateSpy).toHaveBeenCalledWith({
        queryKey: proxyKeys.clientHost,
      });
      expect(invalidateSpy).toHaveBeenCalledWith({
        queryKey: proxyKeys.endpointsInfo,
      });
      expect(invalidateSpy).toHaveBeenCalledWith({
        queryKey: proxyKeys.status,
      });
    });
  });

  it("invalidates proxyKeys.endpointsInfo after deleting a custom preset", async () => {
    const { invalidateSpy } = renderRouting();

    const trigger = await screen.findByRole("combobox");
    fireEvent.keyDown(trigger, { key: "Enter", code: "Enter" });

    const deleteButton = await screen.findByTitle("删除此预设");
    fireEvent.click(deleteButton);

    await waitFor(() => {
      expect(proxyApiMock.deleteCustomListenHost).toHaveBeenCalledWith("::");
      expect(invalidateSpy).toHaveBeenCalledWith({
        queryKey: proxyKeys.endpointsInfo,
      });
    });
  });
});

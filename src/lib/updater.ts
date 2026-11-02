import { getVersion } from "@tauri-apps/api/app";

export type UpdateChannel = "stable" | "beta";

export interface UpdateInfo {
  currentVersion: string;
  availableVersion: string;
  notes?: string;
  pubDate?: string;
}

export interface CheckOptions {
  timeout?: number;
  channel?: UpdateChannel;
}

export async function getCurrentVersion(): Promise<string> {
  try {
    return await getVersion();
  } catch {
    return "";
  }
}

export async function checkForUpdate(
  opts: CheckOptions = {},
): Promise<
  | { status: "up-to-date" }
  | { status: "disabled" }
  | { status: "available"; info: UpdateInfo }
> {
  // 动态引入，避免在未安装插件时导致打包期问题
  const { check } = await import("@tauri-apps/plugin-updater");

  const currentVersion = await getCurrentVersion();
  let update: Awaited<ReturnType<typeof check>>;
  try {
    update = await check({ timeout: opts.timeout ?? 30000 } as any);
  } catch (err) {
    // Fork 构建把 updater endpoints 清空了：官方更新会把带 Tailnet HTTPS 的
    // 二进制覆盖回上游版本，所以这里不是故障，按"已是最新"处理。
    if (String(err).includes("does not have any endpoints set")) {
      return { status: "disabled" };
    }
    throw err;
  }

  if (!update) {
    return { status: "up-to-date" };
  }

  const info: UpdateInfo = {
    currentVersion,
    availableVersion: (update as any).version ?? "",
    notes: (update as any).notes,
    pubDate: (update as any).date,
  };

  return { status: "available", info };
}

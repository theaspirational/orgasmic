import type { ProviderId, ProviderRuntimeEvent } from "./canonical.ts";

export type RuntimeOptions = {
  provider: ProviderId;
  threadId: string;
  cwd: string;
  model?: string;
  effort?: string;
  access?: string;
  serviceTier?: string;
  sandbox?: SandboxPermissions;
};

export type SandboxPermissions = {
  allowExec: boolean;
  allowPatch: boolean;
  allowNetwork: boolean;
  allowWritesOutsideCwd: boolean;
};

export function parseSandboxPermissions(value: string | undefined): SandboxPermissions | undefined {
  if (!value) return undefined;
  const parsed: SandboxPermissions = {
    allowExec: true,
    allowPatch: true,
    allowNetwork: true,
    allowWritesOutsideCwd: true,
  };
  const keys: Record<string, keyof SandboxPermissions> = {
    allow_exec: "allowExec",
    allow_patch: "allowPatch",
    allow_network: "allowNetwork",
    allow_writes_outside_cwd: "allowWritesOutsideCwd",
  };
  for (const entry of value.split(",")) {
    const [rawKey, rawValue] = entry.split("=", 2);
    const key = keys[rawKey?.trim() ?? ""];
    const normalized = rawValue?.trim();
    if (!key || (normalized !== "true" && normalized !== "false")) {
      throw new Error(`Invalid sandbox permission: ${entry}`);
    }
    parsed[key] = normalized === "true";
  }
  return parsed;
}

export type RuntimeOptionPatch = Partial<
  Pick<RuntimeOptions, "model" | "effort" | "access" | "serviceTier">
>;

export type CatalogModel = {
  id: string;
  label: string;
  legacy?: boolean;
  reasoningEfforts?: string[];
};

export type ProviderCatalog = {
  id: ProviderId;
  source: string;
  models: CatalogModel[];
  message?: string;
};

export type Emit = (event: ProviderRuntimeEvent) => void;

export interface ChatRuntime {
  send(text: string): Promise<void>;
  setOptions(patch: RuntimeOptionPatch): Promise<void>;
  stop(reason?: string): Promise<void>;
}

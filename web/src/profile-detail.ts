import type { CatalogProfile } from "./client";

export interface ExternalAssetRequirement {
  id: string;
  kind: string;
  required: boolean;
  note?: string;
}

export interface ProfileDetail {
  id: string;
  machine: string;
  target?: string;
  resources: Record<string, string | number>;
  devices: string[];
  networkMode?: string;
  assetRequirements: ExternalAssetRequirement[];
}

export function profileId(profile: CatalogProfile): string {
  const value = object(profile);
  const metadata = object(value.metadata);
  return text(value.id) ?? text(metadata.name) ?? "unknown";
}

function object(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

function text(value: unknown): string | undefined {
  return typeof value === "string" && value ? value : undefined;
}

/** Select only redistributable profile metadata that is useful to an operator. */
export function profileDetail(profile: CatalogProfile): ProfileDetail {
  const value = object(profile);
  const spec = Object.keys(object(value.spec)).length ? object(value.spec) : value;
  const resources = Object.fromEntries(Object.entries(object(spec.resources)).filter(
    ([, resource]) => typeof resource === "string" || typeof resource === "number",
  )) as Record<string, string | number>;
  const devices = Object.entries(object(spec.devices)).flatMap(([name, enabled]) => enabled === true ? [name] : []);
  const assetRequirements = Array.isArray(spec.external_assets) ? spec.external_assets.flatMap((item) => {
    const asset = object(item);
    const id = text(asset.id);
    const kind = text(asset.kind);
    return id && kind ? [{ id, kind, required: asset.required === true, note: text(asset.note) }] : [];
  }) : [];
  const network = object(spec.network);
  const metadata = object(value.metadata);
  return {
    id: text(value.id) ?? text(metadata.name) ?? "unknown",
    machine: text(spec.machine) ?? "unknown",
    target: text(spec.target),
    resources,
    devices,
    networkMode: text(network.mode),
    assetRequirements,
  };
}

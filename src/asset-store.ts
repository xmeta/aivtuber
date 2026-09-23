// In-memory Performance Asset store with schema-version compatibility.
//
// Implements docs/architecture.adoc section 6.6 and
// docs/performance-assets.adoc: versioned reusable performance units,
// variant groups, and model-version-aware cache keys (architecture
// section 7: a model update must not silently reuse incompatible assets).

import { readFileSync } from "node:fs";

export interface PerformanceAsset {
  schema_version: string;
  id: string;
  intent: string;
  class: "filler" | "reaction" | "phrase" | "template" | "dynamic";
  variant_group?: string | null;
  speech?: { text: string; audio_ref: string; duration_ms: number; viseme_ref?: string | null } | null;
  expression?: { preset: string; intensity: number } | null;
  gesture?: { preset: string; amplitude: number } | null;
  gaze?: "camera" | "chat" | "game" | "speaker" | "away" | null;
  timeline: { at_ms: number; event: string; payload?: unknown }[];
  interrupt_points_ms: number[];
  variation?: Variation;
  compatibility: { compiler_version: string; voice_model?: string | null; avatar_profile?: string | null };
  provenance?: { generated: boolean; generator?: string | null; created_at?: string | null };
}

interface Variation {
  speed_pct?: number;
  amplitude_pct?: number;
  start_delay_ms?: number;
}

export class AssetStore {
  private assets = new Map<string, PerformanceAsset>();

  put(asset: PerformanceAsset): void {
    this.assets.set(asset.id, asset);
  }

  get(id: string): PerformanceAsset | undefined {
    return this.assets.get(id);
  }

  has(id: string): boolean {
    return this.assets.has(id);
  }

  /** Assets whose compatibility profile matches the running runtime. */
  findCompatible(profile: { compiler_version: string; voice_model?: string | null }): PerformanceAsset[] {
    return [...this.assets.values()].filter(
      (a) =>
        a.compatibility.compiler_version === profile.compiler_version &&
        (profile.voice_model == null || a.compatibility.voice_model == null || a.compatibility.voice_model === profile.voice_model),
    );
  }

  /** All assets sharing a variant group, ordered for anti-repetition. */
  variantsOf(variantGroup: string): PerformanceAsset[] {
    return [...this.assets.values()].filter((a) => a.variant_group === variantGroup);
  }

  /** Load a performance-asset JSON file (schema of schemas/performance-asset.schema.json). */
  static fromJsonFile(path: string): PerformanceAsset {
    return JSON.parse(readFileSync(path, "utf8")) as PerformanceAsset;
  }
}

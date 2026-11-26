/**
 * Visual tokens aligned with `DESIGN.md` ("Evidence Before Action").
 */
export interface MasilPalette {
  canvas: string;
  panel: string;
  raised: string;
  text: string;
  textSecondary: string;
  muted: string;
  accent: string;
  accentText: string;
  accentSoft: string;
  accentBorder: string;
  warning: string;
  warningSoft: string;
  danger: string;
  dangerSoft: string;
  success: string;
  successSoft: string;
  track: string;
  hairline: string;
  borderSubtle: string;
  segmentTrack: string;
  segmentThumb: string;
  disabledBg: string;
  disabledText: string;
  isDark: boolean;
}

export const DARK_PALETTE: MasilPalette = {
  canvas: "#0c1016",
  panel: "#141a22",
  raised: "#1f2731",
  text: "#e6ebf1",
  textSecondary: "#b5c2d1",
  muted: "#95a3b3",
  accent: "#5bb5c2",
  accentText: "#051316",
  accentSoft: "rgba(91, 181, 194, 0.14)",
  accentBorder: "rgba(91, 181, 194, 0.48)",
  warning: "#ebb85c",
  warningSoft: "rgba(235, 184, 92, 0.14)",
  danger: "#e86f75",
  dangerSoft: "rgba(232, 111, 117, 0.14)",
  success: "#74c48a",
  successSoft: "rgba(116, 196, 138, 0.14)",
  track: "#303a45",
  hairline: "rgba(230, 235, 241, 0.08)",
  borderSubtle: "rgba(230, 235, 241, 0.14)",
  segmentTrack: "#0f141b",
  segmentThumb: "#222c38",
  disabledBg: "#1b232d",
  disabledText: "#8a98a8",
  isDark: true,
};

export const LIGHT_PALETTE: MasilPalette = {
  canvas: "#eef2f4",
  panel: "#fafbfb",
  raised: "#dce3e6",
  text: "#192229",
  textSecondary: "#3d4d58",
  muted: "#54646f",
  accent: "#00707f",
  accentText: "#ffffff",
  accentSoft: "rgba(0, 112, 127, 0.10)",
  accentBorder: "rgba(0, 112, 127, 0.42)",
  warning: "#905400",
  warningSoft: "rgba(144, 84, 0, 0.10)",
  danger: "#aa2a34",
  dangerSoft: "rgba(170, 42, 52, 0.10)",
  success: "#167036",
  successSoft: "rgba(22, 112, 54, 0.10)",
  track: "#becacf",
  hairline: "rgba(25, 34, 41, 0.08)",
  borderSubtle: "rgba(25, 34, 41, 0.14)",
  segmentTrack: "#e2e8ec",
  segmentThumb: "#ffffff",
  disabledBg: "#e1e7ea",
  disabledText: "#5f6f7b",
  isDark: false,
};

export type ThemePreference = "system" | "dark" | "light";

export interface PluginThemeColorsInput {
  appearance: "dark" | "light" | string;
  canvas: string;
  panel: string;
  raised: string;
  text: string;
  muted: string;
  accent: string;
  accent_text: string;
  warning: string;
  danger: string;
  success: string;
  track: string;
}

function parseHex(hex: string): [number, number, number] | null {
  const clean = hex.trim().replace(/^#/, "");
  if (!/^[0-9a-fA-F]{6}$/.test(clean)) return null;
  return [
    parseInt(clean.slice(0, 2), 16),
    parseInt(clean.slice(2, 4), 16),
    parseInt(clean.slice(4, 6), 16),
  ];
}

function withAlpha(hex: string, alpha: number, fallback: string): string {
  const rgb = parseHex(hex);
  if (!rgb) return fallback;
  return `rgba(${rgb[0]}, ${rgb[1]}, ${rgb[2]}, ${alpha})`;
}

function mixHex(a: string, b: string, weight: number, fallback: string): string {
  const ra = parseHex(a);
  const rb = parseHex(b);
  if (!ra || !rb) return fallback;
  const ch = (i: number) =>
    Math.max(
      0,
      Math.min(255, Math.round(ra[i]! * (1 - weight) + rb[i]! * weight))
    )
      .toString(16)
      .padStart(2, "0");
  return `#${ch(0)}${ch(1)}${ch(2)}`;
}

/**
 * Expands an Omarchy-style 11-color plugin theme into the full 24-token
 * MasilPalette used across every mobile surface.
 */
export function expandPluginThemeToPalette(
  theme: PluginThemeColorsInput
): MasilPalette {
  const isDark = theme.appearance !== "light";
  const base = isDark ? DARK_PALETTE : LIGHT_PALETTE;
  const softAlpha = isDark ? 0.14 : 0.1;

  return {
    canvas: theme.canvas || base.canvas,
    panel: theme.panel || base.panel,
    raised: theme.raised || base.raised,
    text: theme.text || base.text,
    textSecondary: mixHex(theme.text, theme.muted, 0.35, base.textSecondary),
    muted: theme.muted || base.muted,
    accent: theme.accent || base.accent,
    accentText: theme.accent_text || base.accentText,
    accentSoft: withAlpha(theme.accent, softAlpha, base.accentSoft),
    accentBorder: withAlpha(theme.accent, isDark ? 0.48 : 0.42, base.accentBorder),
    warning: theme.warning || base.warning,
    warningSoft: withAlpha(theme.warning, softAlpha, base.warningSoft),
    danger: theme.danger || base.danger,
    dangerSoft: withAlpha(theme.danger, softAlpha, base.dangerSoft),
    success: theme.success || base.success,
    successSoft: withAlpha(theme.success, softAlpha, base.successSoft),
    track: theme.track || base.track,
    hairline: withAlpha(theme.text, 0.08, base.hairline),
    borderSubtle: withAlpha(theme.text, 0.15, base.borderSubtle),
    segmentTrack: mixHex(theme.canvas, theme.panel, 0.45, base.segmentTrack),
    segmentThumb: isDark ? theme.raised || base.segmentThumb : "#ffffff",
    disabledBg: mixHex(theme.panel, theme.raised, 0.5, base.disabledBg),
    disabledText: theme.muted || base.disabledText,
    isDark,
  };
}

export interface ThemeLookupItem extends PluginThemeColorsInput {
  id: string;
}

export interface ResolvedThemeSelection {
  palette: MasilPalette;
  preference: ThemePreference;
  activeThemeId: string | null;
}

export function resolveThemeSelection(input: {
  preference: ThemePreference;
  systemScheme: string | null | undefined;
  localPluginThemeId: string | null;
  hostActiveTheme: string | null;
  availablePluginThemes: ThemeLookupItem[];
}): ResolvedThemeSelection {
  const requestedThemeId = input.localPluginThemeId ?? input.hostActiveTheme;

  if (
    requestedThemeId &&
    requestedThemeId !== "dark" &&
    requestedThemeId !== "light" &&
    requestedThemeId !== "system" &&
    requestedThemeId !== "terminal"
  ) {
    const matched = input.availablePluginThemes.find(
      (t) => t.id === requestedThemeId
    );
    if (matched) {
      return {
        palette: expandPluginThemeToPalette(matched),
        preference: input.preference,
        activeThemeId: matched.id,
      };
    }
  }

  const effectivePref: ThemePreference =
    requestedThemeId === "light" ||
    requestedThemeId === "dark" ||
    requestedThemeId === "system"
      ? requestedThemeId
      : input.preference;
  const isDark =
    effectivePref === "dark" ||
    (effectivePref === "system" && input.systemScheme !== "light");
  return {
    palette: isDark ? DARK_PALETTE : LIGHT_PALETTE,
    preference: effectivePref,
    activeThemeId: requestedThemeId,
  };
}

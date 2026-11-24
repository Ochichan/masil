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

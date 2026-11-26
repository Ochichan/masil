import { describe, expect, it } from "vitest";
import {
  DARK_PALETTE,
  LIGHT_PALETTE,
  expandPluginThemeToPalette,
  resolveThemeSelection,
} from "./tokens";
import type { PluginThemeDef } from "../protocol/schemas";

const SAMPLE_THEME: PluginThemeDef = {
  id: "tokyo-night",
  plugin_id: "tokyo-night",
  name: "Tokyo Night",
  appearance: "dark",
  canvas: "#1a1b26",
  panel: "#16161e",
  raised: "#24283b",
  text: "#c0caf5",
  muted: "#565f89",
  accent: "#7aa2f7",
  accent_text: "#15161e",
  warning: "#e0af68",
  danger: "#f7768e",
  success: "#9ece6a",
  track: "#3b4261",
};

describe("expandPluginThemeToPalette", () => {
  it("expands a dark plugin theme into a full MasilPalette", () => {
    const palette = expandPluginThemeToPalette({
      appearance: "dark",
      canvas: "#1e1e2e",
      panel: "#181825",
      raised: "#313244",
      text: "#cdd6f4",
      muted: "#a6adc8",
      accent: "#cba6f7",
      accent_text: "#11111b",
      warning: "#f9e2af",
      danger: "#f38ba8",
      success: "#a6e3a1",
      track: "#45475a",
    });

    expect(palette.isDark).toBe(true);
    expect(palette.canvas).toBe("#1e1e2e");
    expect(palette.accent).toBe("#cba6f7");
    expect(palette.accentSoft).toBe("rgba(203, 166, 247, 0.14)");
    expect(palette.hairline).toBe("rgba(205, 214, 244, 0.08)");
  });

  it("expands a light plugin theme cleanly", () => {
    const palette = expandPluginThemeToPalette({
      appearance: "light",
      canvas: "#eff1f5",
      panel: "#ffffff",
      raised: "#dce0e8",
      text: "#4c4f69",
      muted: "#6c6f85",
      accent: "#8839ef",
      accent_text: "#ffffff",
      warning: "#df8e1d",
      danger: "#d20f39",
      success: "#40a02b",
      track: "#ccd0da",
    });

    expect(palette.isDark).toBe(false);
    expect(palette.segmentThumb).toBe("#ffffff");
    expect(palette.accentSoft).toBe("rgba(136, 57, 239, 0.1)");
  });
});

describe("resolveThemeSelection", () => {
  it("applies host-published plugin theme when no local override is set", () => {
    const res = resolveThemeSelection({
      preference: "dark",
      systemScheme: "dark",
      localPluginThemeId: null,
      hostActiveTheme: "tokyo-night",
      availablePluginThemes: [SAMPLE_THEME],
    });
    expect(res.activeThemeId).toBe("tokyo-night");
    expect(res.palette.accent).toBe("#7aa2f7");
  });

  it("falls back to light/dark built-in palette when host resets theme", () => {
    const res = resolveThemeSelection({
      preference: "dark",
      systemScheme: "dark",
      localPluginThemeId: null,
      hostActiveTheme: "light",
      availablePluginThemes: [SAMPLE_THEME],
    });
    expect(res.palette).toBe(LIGHT_PALETTE);
    expect(res.preference).toBe("light");

    const backDark = resolveThemeSelection({
      preference: "dark",
      systemScheme: "dark",
      localPluginThemeId: null,
      hostActiveTheme: null,
      availablePluginThemes: [SAMPLE_THEME],
    });
    expect(backDark.palette).toBe(DARK_PALETTE);
  });
});

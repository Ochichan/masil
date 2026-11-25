import { describe, expect, it } from "vitest";
import { expandPluginThemeToPalette } from "./tokens";

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

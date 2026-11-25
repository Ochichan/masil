import React, { createContext, useContext, useMemo, useState } from "react";
import { useColorScheme } from "react-native";
import { useHostRuntimeStore } from "../hosts/HostRuntimeController";
import type { PluginThemeDef } from "../protocol/schemas";
import {
  DARK_PALETTE,
  LIGHT_PALETTE,
  MasilPalette,
  ThemePreference,
  expandPluginThemeToPalette,
} from "./tokens";

interface ThemeContextValue {
  palette: MasilPalette;
  preference: ThemePreference;
  activeThemeId: string | null;
  availablePluginThemes: PluginThemeDef[];
  setPreference: (pref: ThemePreference) => void;
  setLocalPluginTheme: (themeId: string | null) => void;
}

const ThemeContext = createContext<ThemeContextValue>({
  palette: DARK_PALETTE,
  preference: "dark",
  activeThemeId: null,
  availablePluginThemes: [],
  setPreference: () => {},
  setLocalPluginTheme: () => {},
});

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const system = useColorScheme();
  const [preference, setPreference] = useState<ThemePreference>("dark");
  const [localPluginThemeId, setLocalPluginTheme] = useState<string | null>(
    null
  );

  const activeHostId = useHostRuntimeStore((s) => s.activeHostId);
  const hostPlugins = useHostRuntimeStore((s) =>
    s.activeHostId ? s.hosts[s.activeHostId]?.plugins ?? null : null
  );

  const availablePluginThemes = useMemo(
    () => hostPlugins?.themes ?? [],
    [hostPlugins]
  );

  const hostActiveTheme = hostPlugins?.active_theme ?? null;

  const value = useMemo<ThemeContextValue>(() => {
    const requestedThemeId = localPluginThemeId ?? hostActiveTheme;

    if (
      requestedThemeId &&
      requestedThemeId !== "dark" &&
      requestedThemeId !== "light" &&
      requestedThemeId !== "system" &&
      requestedThemeId !== "terminal"
    ) {
      const matched = availablePluginThemes.find(
        (t) => t.id === requestedThemeId
      );
      if (matched) {
        return {
          palette: expandPluginThemeToPalette(matched),
          preference,
          activeThemeId: matched.id,
          availablePluginThemes,
          setPreference,
          setLocalPluginTheme,
        };
      }
    }

    const effectivePref: ThemePreference =
      requestedThemeId === "light" ||
      requestedThemeId === "dark" ||
      requestedThemeId === "system"
        ? requestedThemeId
        : preference;
    const isDark =
      effectivePref === "dark" ||
      (effectivePref === "system" && system !== "light");
    return {
      palette: isDark ? DARK_PALETTE : LIGHT_PALETTE,
      preference: effectivePref,
      activeThemeId: requestedThemeId,
      availablePluginThemes,
      setPreference: (next) => {
        setLocalPluginTheme(null);
        setPreference(next);
      },
      setLocalPluginTheme,
    };
  }, [
    preference,
    system,
    localPluginThemeId,
    hostActiveTheme,
    availablePluginThemes,
    activeHostId,
  ]);

  return (
    <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>
  );
}

export function useMasilTheme(): ThemeContextValue {
  return useContext(ThemeContext);
}

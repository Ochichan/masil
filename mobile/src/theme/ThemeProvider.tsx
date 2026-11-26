import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
} from "react";
import { useColorScheme } from "react-native";
import { useHostRuntimeStore } from "../hosts/HostRuntimeController";
import type { PluginThemeDef } from "../protocol/schemas";
import {
  DARK_PALETTE,
  MasilPalette,
  ThemePreference,
  resolveThemeSelection,
} from "./tokens";

export { resolveThemeSelection } from "./tokens";
export type { ResolvedThemeSelection } from "./tokens";

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
  const [preference, setPreferenceState] = useState<ThemePreference>("dark");
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

  // Whenever the host publishes a new active theme (from CLI, TUI, AI session,
  // or another device) or the user switches hosts, drop any optimistic local
  // override so the live host theme takes effect immediately.
  useEffect(() => {
    setLocalPluginTheme(null);
  }, [hostActiveTheme, activeHostId]);

  const handleSetPreference = useCallback((next: ThemePreference) => {
    setLocalPluginTheme(null);
    setPreferenceState(next);
  }, []);

  const value = useMemo<ThemeContextValue>(() => {
    const resolved = resolveThemeSelection({
      preference,
      systemScheme: system,
      localPluginThemeId,
      hostActiveTheme,
      availablePluginThemes,
    });
    return {
      palette: resolved.palette,
      preference: resolved.preference,
      activeThemeId: resolved.activeThemeId,
      availablePluginThemes,
      setPreference: handleSetPreference,
      setLocalPluginTheme,
    };
  }, [
    preference,
    system,
    localPluginThemeId,
    hostActiveTheme,
    availablePluginThemes,
    handleSetPreference,
  ]);

  return (
    <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>
  );
}

export function useMasilTheme(): ThemeContextValue {
  return useContext(ThemeContext);
}

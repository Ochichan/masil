import React, { createContext, useContext, useMemo, useState } from "react";
import { useColorScheme } from "react-native";
import {
  DARK_PALETTE,
  LIGHT_PALETTE,
  MasilPalette,
  ThemePreference,
} from "./tokens";

interface ThemeContextValue {
  palette: MasilPalette;
  preference: ThemePreference;
  setPreference: (pref: ThemePreference) => void;
}

const ThemeContext = createContext<ThemeContextValue>({
  palette: DARK_PALETTE,
  preference: "dark",
  setPreference: () => {},
});

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const system = useColorScheme();
  const [preference, setPreference] = useState<ThemePreference>("dark");

  const value = useMemo<ThemeContextValue>(() => {
    const isDark =
      preference === "dark" ||
      (preference === "system" && system !== "light");
    return {
      palette: isDark ? DARK_PALETTE : LIGHT_PALETTE,
      preference,
      setPreference,
    };
  }, [preference, system]);

  return (
    <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>
  );
}

export function useMasilTheme(): ThemeContextValue {
  return useContext(ThemeContext);
}

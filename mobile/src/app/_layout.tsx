import "../polyfills";
import React, { useEffect } from "react";
import { useFonts } from "expo-font";
import { Stack } from "expo-router";
import { GestureHandlerRootView } from "react-native-gesture-handler";
import { KeyboardProvider } from "react-native-keyboard-controller";
import { SafeAreaProvider } from "react-native-safe-area-context";
import { useHostRuntimeStore } from "../hosts/HostRuntimeController";
import { initI18n } from "../i18n";
import {
  setupNotificationSystem,
  subscribeNotificationDeepLinks,
} from "../notifications/push-manager";
import { ThemeProvider } from "../theme/ThemeProvider";

initI18n("ko");

export default function RootLayout() {
  const bootstrap = useHostRuntimeStore((s) => s.bootstrap);
  const [fontsLoaded, fontError] = useFonts({
    "Pretendard-Regular": require("../../assets/fonts/Pretendard-Regular.otf"),
    "Pretendard-SemiBold": require("../../assets/fonts/Pretendard-SemiBold.otf"),
    "Pretendard-Bold": require("../../assets/fonts/Pretendard-Bold.otf"),
    "CascadiaMono-Regular": require("../../assets/fonts/CascadiaMono-Regular.ttf"),
    "CascadiaMono-SemiBold": require("../../assets/fonts/CascadiaMono-SemiBold.ttf"),
  });

  useEffect(() => {
    void setupNotificationSystem();
    void bootstrap();
    return subscribeNotificationDeepLinks();
  }, [bootstrap]);

  if (!fontsLoaded && !fontError) {
    return null;
  }

  return (
    <GestureHandlerRootView style={{ flex: 1 }}>
      <SafeAreaProvider>
        <KeyboardProvider>
          <ThemeProvider>
            <Stack screenOptions={{ headerShown: false }}>
              <Stack.Screen name="index" />
              <Stack.Screen
                name="pair"
                options={{ presentation: "modal" }}
              />
              <Stack.Screen
                name="settings"
                options={{ presentation: "modal" }}
              />
            </Stack>
          </ThemeProvider>
        </KeyboardProvider>
      </SafeAreaProvider>
    </GestureHandlerRootView>
  );
}

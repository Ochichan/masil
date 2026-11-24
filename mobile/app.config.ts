import type { ExpoConfig } from "expo/config";

const appVariant = process.env.APP_VARIANT ?? "production";

const variants = {
  production: {
    name: "masil",
    bundleId: "dev.masil.app",
  },
  development: {
    name: "masil Debug",
    bundleId: "dev.masil.app.debug",
  },
} as const;

const variant =
  appVariant === "development" ? variants.development : variants.production;

const config: ExpoConfig = {
  name: variant.name,
  slug: "masil-mobile",
  version: "0.1.0",
  orientation: "default",
  scheme: "masil",
  userInterfaceStyle: "automatic",
  icon: "./assets/icon.png",
  ios: {
    supportsTablet: true,
    bundleIdentifier: variant.bundleId,
    infoPlist: {
      NSCameraUsageDescription:
        "Allow masil to scan host QR codes for end-to-end encrypted pairing.",
      NSMicrophoneUsageDescription:
        "Allow masil to record voice dictation for agent prompts.",
      NSSpeechRecognitionUsageDescription:
        "Allow masil to transcribe voice dictation on-device.",
      ITSAppUsesNonExemptEncryption: false,
    },
  },
  android: {
    package: variant.bundleId,
    adaptiveIcon: {
      foregroundImage: "./assets/adaptive-icon.png",
      backgroundColor: "#0e1620",
    },
    softwareKeyboardLayoutMode: "resize",
    permissions: [
      "CAMERA",
      "RECORD_AUDIO",
      "POST_NOTIFICATIONS",
      "INTERNET",
    ],
  },
  plugins: [
    "expo-router",
    [
      "expo-font",
      {
        fonts: [
          "./assets/fonts/Pretendard-Regular.otf",
          "./assets/fonts/Pretendard-SemiBold.otf",
          "./assets/fonts/Pretendard-Bold.otf",
          "./assets/fonts/CascadiaMono-Regular.ttf",
          "./assets/fonts/CascadiaMono-SemiBold.ttf",
        ],
      },
    ],
    "expo-secure-store",
    "expo-sqlite",
    "expo-audio",
    [
      "expo-camera",
      {
        cameraPermission:
          "Allow masil to scan host QR codes for end-to-end encrypted pairing.",
      },
    ],
    [
      "expo-notifications",
      {
        color: "#5bb5c2",
      },
    ],
    [
      "expo-build-properties",
      {
        android: {
          minSdkVersion: 29,
          usesCleartextTraffic: true,
        },
        ios: {
          deploymentTarget: "16.4",
        },
      },
    ],
  ],
  experiments: {
    typedRoutes: true,
  },
  extra: {
    appVariant,
  },
};

export default config;

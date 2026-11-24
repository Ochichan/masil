#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
export APP_VARIANT="${APP_VARIANT:-production}"

CI=1 npx expo prebuild --platform android --clean
mkdir -p artifacts
(
  cd android
  ./gradlew assembleRelease -PreactNativeArchitectures=arm64-v8a
)
APK_SRC="android/app/build/outputs/apk/release/app-release.apk"
if [ -f "$APK_SRC" ]; then
  cp "$APK_SRC" "artifacts/masil-${APP_VARIANT}-arm64.apk"
  echo "Built mobile/artifacts/masil-${APP_VARIANT}-arm64.apk"
else
  echo "Expected APK not found at $APK_SRC" >&2
  exit 1
fi

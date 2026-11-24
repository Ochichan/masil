#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
export APP_VARIANT="${APP_VARIANT:-development}"

CI=1 npx expo prebuild --platform ios --clean
(
  cd ios
  pod install
)
echo "iOS workspace prepared. Open mobile/ios/*.xcworkspace in Xcode or run: npm run ios"

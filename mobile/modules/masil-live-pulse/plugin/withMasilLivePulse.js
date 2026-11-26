/**
 * Expo Config Plugin for `masil-live-pulse` (Phase 2B / D-09):
 * - Configures `NSSupportsLiveActivities = true` and App Group (`group.<bundleId>`)
 *   + APNs push entitlements on the main iOS app target.
 * - Stages `MasilLiveActivityExtension` SwiftUI WidgetKit Extension files and
 *   registers the extension target during `expo prebuild --platform ios`.
 * - Honors `ENABLE_LIVE_ACTIVITIES=0` for unsigned external contributor builds,
 *   skipping Widget Extension target creation and falling back safely to local notifications.
 */
const fs = require("node:fs");
const path = require("node:path");
const {
  withEntitlementsPlist,
  withInfoPlist,
  withXcodeProject,
  withDangerousMod,
} = require("expo/config-plugins");

const EXTENSION_NAME = "MasilLiveActivityExtension";

function withMasilLivePulse(config, options = {}) {
  const enabledByEnv = process.env.ENABLE_LIVE_ACTIVITIES !== "0";
  const enableLiveActivities =
    options.enabled !== undefined ? Boolean(options.enabled) : enabledByEnv;

  const bundleId = config.ios?.bundleIdentifier ?? "dev.masil.app";
  const appGroupId = options.appGroup ?? `group.${bundleId}`;
  const extensionBundleId = `${bundleId}.LiveActivity`;

  config = withInfoPlist(config, (mod) => {
    mod.modResults.NSSupportsLiveActivities = enableLiveActivities;
    if (enableLiveActivities) {
      mod.modResults.NSSupportsLiveActivitiesFrequentUpdates = true;
    }
    return mod;
  });

  if (!enableLiveActivities) {
    return config;
  }

  config = withEntitlementsPlist(config, (mod) => {
    const groups = Array.isArray(mod.modResults["com.apple.security.application-groups"])
      ? mod.modResults["com.apple.security.application-groups"]
      : [];
    if (!groups.includes(appGroupId)) {
      groups.push(appGroupId);
    }
    mod.modResults["com.apple.security.application-groups"] = groups;
    if (!mod.modResults["aps-environment"]) {
      mod.modResults["aps-environment"] =
        process.env.APP_VARIANT === "development" ? "development" : "production";
    }
    return mod;
  });

  // Stage Widget Extension Swift sources + Info.plist + entitlements into `ios/MasilLiveActivityExtension/`.
  config = withDangerousMod(config, [
    "ios",
    async (mod) => {
      const iosRoot = mod.modRequest.platformProjectRoot;
      const destDir = path.join(iosRoot, EXTENSION_NAME);
      fs.mkdirSync(destDir, { recursive: true });

      const srcRoot = path.resolve(__dirname, "..", "ios");
      const filesToCopy = [
        [path.join(srcRoot, "MasilFleetAttributes.swift"), path.join(destDir, "MasilFleetAttributes.swift")],
        [path.join(srcRoot, "WidgetExtension", "MasilLiveActivityWidget.swift"), path.join(destDir, "MasilLiveActivityWidget.swift")],
        [path.join(srcRoot, "WidgetExtension", "MasilLiveActivityBundle.swift"), path.join(destDir, "MasilLiveActivityBundle.swift")],
        [path.join(srcRoot, "WidgetExtension", "Info.plist"), path.join(destDir, "Info.plist")],
      ];
      for (const [src, dst] of filesToCopy) {
        if (fs.existsSync(src)) {
          fs.copyFileSync(src, dst);
        }
      }

      const extEntitlements = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.application-groups</key>
  <array>
    <string>${appGroupId}</string>
  </array>
</dict>
</plist>
`;
      fs.writeFileSync(
        path.join(destDir, `${EXTENSION_NAME}.entitlements`),
        extEntitlements,
        "utf8"
      );
      return mod;
    },
  ]);

  // Register `MasilLiveActivityExtension` target in the generated Xcode project (idempotent).
  config = withXcodeProject(config, (mod) => {
    const proj = mod.modResults;
    if (proj.pbxTargetByName(EXTENSION_NAME)) {
      return mod;
    }

    const group = proj.addPbxGroup(
      [
        "MasilFleetAttributes.swift",
        "MasilLiveActivityWidget.swift",
        "MasilLiveActivityBundle.swift",
        "Info.plist",
        `${EXTENSION_NAME}.entitlements`,
      ],
      EXTENSION_NAME,
      EXTENSION_NAME
    );

    const rootGroupKey = proj.getFirstProject().firstProject.mainGroup;
    proj.addToPbxGroup(group.uuid, rootGroupKey);

    const target = proj.addTarget(
      EXTENSION_NAME,
      "app_extension",
      EXTENSION_NAME,
      extensionBundleId
    );

    proj.addBuildPhase(
      [
        `${EXTENSION_NAME}/MasilFleetAttributes.swift`,
        `${EXTENSION_NAME}/MasilLiveActivityWidget.swift`,
        `${EXTENSION_NAME}/MasilLiveActivityBundle.swift`,
      ],
      "PBXSourcesBuildPhase",
      "Sources",
      target.uuid
    );
    proj.addBuildPhase([], "PBXResourcesBuildPhase", "Resources", target.uuid);
    proj.addBuildPhase(
      ["WidgetKit.framework", "SwiftUI.framework", "ActivityKit.framework"],
      "PBXFrameworksBuildPhase",
      "Frameworks",
      target.uuid
    );

    const configs = proj.pbxXCBuildConfigurationSection();
    for (const key of Object.keys(configs)) {
      const cfg = configs[key];
      if (
        cfg &&
        cfg.buildSettings &&
        cfg.buildSettings.PRODUCT_NAME === `"${EXTENSION_NAME}"`
      ) {
        cfg.buildSettings.IPHONEOS_DEPLOYMENT_TARGET = "16.4";
        cfg.buildSettings.SWIFT_VERSION = "5.9";
        cfg.buildSettings.TARGETED_DEVICE_FAMILY = `"1,2"`;
        cfg.buildSettings.INFOPLIST_FILE = `"${EXTENSION_NAME}/Info.plist"`;
        cfg.buildSettings.CODE_SIGN_ENTITLEMENTS = `"${EXTENSION_NAME}/${EXTENSION_NAME}.entitlements"`;
        cfg.buildSettings.PRODUCT_BUNDLE_IDENTIFIER = `"${extensionBundleId}"`;
        cfg.buildSettings.GENERATE_INFOPLIST_FILE = "NO";
      }
    }

    return mod;
  });

  return config;
}

module.exports = withMasilLivePulse;

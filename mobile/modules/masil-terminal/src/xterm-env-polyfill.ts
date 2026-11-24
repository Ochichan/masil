/**
 * `@xterm/headless` evaluates `navigator.userAgent.includes(...)` and
 * `navigator.platform.indexOf(...)` at module load time. In React Native Hermes,
 * `navigator` exists (`{ product: "ReactNative" }`) without `userAgent` or
 * `platform`, which throws on import unless initialized first.
 */
const globalObj = globalThis as unknown as {
  navigator?: Record<string, unknown>;
};

const nav = globalObj.navigator;
if (nav && typeof nav === "object") {
  if (typeof nav.userAgent !== "string") {
    try {
      nav.userAgent = "ReactNative/masil";
    } catch {
      Object.defineProperty(nav, "userAgent", {
        value: "ReactNative/masil",
        configurable: true,
      });
    }
  }
  if (typeof nav.platform !== "string") {
    try {
      nav.platform = "ReactNative";
    } catch {
      Object.defineProperty(nav, "platform", {
        value: "ReactNative",
        configurable: true,
      });
    }
  }
} else {
  globalObj.navigator = {
    userAgent: "ReactNative/masil",
    platform: "ReactNative",
  };
}

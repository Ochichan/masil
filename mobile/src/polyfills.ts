/**
 * Runtime polyfills executed before route modules evaluate in React Native
 * Hermes:
 * 1. `navigator.userAgent` & `navigator.platform` for `@xterm/headless`.
 * 2. `globalThis.crypto.getRandomValues` backed by `expo-crypto` for `@noble/*`.
 */
import "../modules/masil-terminal/src/xterm-env-polyfill";
import * as ExpoCrypto from "expo-crypto";

const g = globalThis as unknown as {
  crypto?: {
    getRandomValues?: <T extends ArrayBufferView>(array: T) => T;
  };
};

if (!g.crypto) {
  g.crypto = {};
}
if (typeof g.crypto.getRandomValues !== "function") {
  g.crypto.getRandomValues = <T extends ArrayBufferView>(array: T): T => {
    const bytes = new Uint8Array(
      array.buffer,
      array.byteOffset,
      array.byteLength
    );
    ExpoCrypto.getRandomValues(bytes);
    return array;
  };
}

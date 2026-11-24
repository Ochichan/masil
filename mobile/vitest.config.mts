import { defineConfig } from "vitest/config";
import path from "node:path";

export default defineConfig({
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "src"),
      "masil-terminal": path.resolve(
        import.meta.dirname,
        "modules/masil-terminal/src/index.tsx"
      ),
    },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts", "modules/**/*.test.ts"],
  },
});

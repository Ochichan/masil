/**
 * Bundled typography tokens (Pretendard for Korean + Latin UI sans,
 * CascadiaMono for paths, model slugs, diffs, and tabular counts).
 * Explicitly setting fontFamily overrides custom Android system fonts.
 */
export const FONTS = {
  regular: "Pretendard-Regular",
  semi: "Pretendard-SemiBold",
  bold: "Pretendard-Bold",
  mono: "CascadiaMono-Regular",
  monoSemi: "CascadiaMono-SemiBold",
} as const;

export const TABULAR_NUMS = ["tabular-nums"] as ("tabular-nums")[];

export function shortenHomePath(raw: string | null | undefined): string {
  if (!raw) return "";
  const trimmed = raw.trim();
  const homeMatch = trimmed.match(/^\/(?:Users|home)\/[^/]+(\/.*)?$/);
  if (homeMatch) {
    return `~${homeMatch[1] ?? ""}`;
  }
  return trimmed;
}

export function isScratchDirectory(path: string, name: string): boolean {
  const lowerPath = path.toLowerCase();
  const lowerName = name.toLowerCase();
  return (
    lowerPath.startsWith("/tmp") ||
    lowerPath.startsWith("/private/tmp") ||
    lowerPath.startsWith("/var/folders") ||
    lowerPath.startsWith("/private/var/folders") ||
    lowerName.startsWith("uiclimb-rv-") ||
    lowerName.startsWith("tmp.")
  );
}

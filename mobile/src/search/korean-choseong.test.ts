import { describe, expect, it } from "vitest";
import { matchesMasilQuery } from "./korean-choseong";

describe("Korean initial consonant search", () => {
  it("matches choseong queries and multi-word filters", () => {
    expect(matchesMasilQuery("개발서버 claude", "ㄱㅂ")).toBe(true);
    expect(matchesMasilQuery("개발서버 claude", "ㄱㅂ claude")).toBe(true);
    expect(matchesMasilQuery("개발서버 claude", "ㄱㅂ codex")).toBe(false);
    expect(matchesMasilQuery("backend-api", "back api")).toBe(true);
  });
});

import { describe, expect, it } from "vitest";
import { encodeTerminalFrame } from "./frame-encoding";
import { HeadlessTerminalEngine } from "./xterm-adapter";

describe("masil-terminal frame encoder & xterm grapheme engine", () => {
  it("renders Hangul and ANSI colors into encoded cell frames", async () => {
    const engine = new HeadlessTerminalEngine(40, 6);
    await new Promise<void>((resolve) => {
      engine.write("\x1b[32m한글 테스트 OK\x1b[0m\r\n", resolve);
    });
    const frame = engine.snapshotFrame(0xffe6ebf1, 0xff0c1016);
    expect(frame.columns).toBe(40);
    expect(frame.rows.length).toBe(6);
    expect(frame.rows[0]!.text.startsWith("한글 테스트 OK")).toBe(true);
    // Hangul characters occupy 2 columns each in the cell layout.
    expect(frame.rows[0]!.cells[0]).toBe(1);
    expect(frame.rows[0]!.cells[1]).toBe(2);

    const encoded = encodeTerminalFrame(frame);
    expect(encoded.rows[0]!.includes(";한글 테스트 OK")).toBe(true);
    engine.dispose();
  });
});

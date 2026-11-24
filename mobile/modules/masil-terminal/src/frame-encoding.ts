import type { MasilTerminalCursor, MasilTerminalFrame } from "./contract";

export interface NativeTerminalFrame {
  columns: number;
  /** Encoded as `cellsCsv;stylesCsv;text` so unchanged rows hit native shaping cache in O(1). */
  rows: string[];
  cursor?: MasilTerminalCursor;
}

export function encodeTerminalFrame(
  frame: MasilTerminalFrame
): NativeTerminalFrame {
  const native: NativeTerminalFrame = {
    columns: frame.columns,
    rows: frame.rows.map(
      (row) => `${row.cells.join(",")};${row.styles.join(",")};${row.text}`
    ),
  };
  if (frame.cursor) {
    native.cursor = frame.cursor;
  }
  return native;
}

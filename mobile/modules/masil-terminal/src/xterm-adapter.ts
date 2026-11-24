import "./xterm-env-polyfill";
import { Terminal } from "@xterm/headless";
import { UnicodeGraphemesAddon } from "@xterm/addon-unicode-graphemes";
import {
  MasilTerminalFrame,
  MasilTerminalRow,
  TERMINAL_STYLE_FLAGS,
} from "./contract";

const ANSI_16: readonly number[] = [
  0xff0c1016, 0xffe86f75, 0xff74c48a, 0xffebb85c, 0xff5bb5c2, 0xffb388eb,
  0xff5bb5c2, 0xffe6ebf1, 0xff4c5968, 0xffff898e, 0xff93dba7, 0xfff5cb7c,
  0xff79cedb, 0xffcca7ff, 0xff7ad8e6, 0xffffffff,
];

function ansi256ToArgb(idx: number): number {
  if (idx < 16) return ANSI_16[idx] ?? 0xffe6ebf1;
  if (idx >= 232) {
    const v = 8 + (idx - 232) * 10;
    return (0xff000000 | (v << 16) | (v << 8) | v) >>> 0;
  }
  const n = idx - 16;
  const r = Math.floor(n / 36);
  const g = Math.floor((n % 36) / 6);
  const b = n % 6;
  const scale = (c: number) => (c === 0 ? 0 : 55 + c * 40);
  return (
    (0xff000000 | (scale(r) << 16) | (scale(g) << 8) | scale(b)) >>> 0
  );
}

export class HeadlessTerminalEngine {
  readonly term: Terminal;

  constructor(cols: number, rows: number) {
    this.term = new Terminal({
      cols,
      rows,
      scrollback: 1000,
      allowProposedApi: true,
    });
    const graphemes = new UnicodeGraphemesAddon();
    this.term.loadAddon(graphemes);
    this.term.unicode.activeVersion = "15-graphemes";
  }

  write(data: Uint8Array | string, onDone?: () => void): void {
    this.term.write(data, onDone);
  }

  resize(cols: number, rows: number): void {
    this.term.resize(Math.max(20, cols), Math.max(6, rows));
  }

  reset(): void {
    this.term.reset();
  }

  snapshotFrame(defaultFg: number, defaultBg: number): MasilTerminalFrame {
    const buf = this.term.buffer.active;
    const rows: MasilTerminalRow[] = [];
    const cols = this.term.cols;
    const scratchCell = buf.getNullCell();

    for (let y = 0; y < this.term.rows; y++) {
      const line = buf.getLine(buf.viewportY + y);
      let text = "";
      const cells: number[] = [];
      const styles: number[] = [];

      let spanCols = 0;
      let spanFg = defaultFg;
      let spanBg = defaultBg;
      let spanFlags = 0;

      for (let x = 0; x < cols; x++) {
        const cell = line?.getCell(x, scratchCell);
        const width = cell ? cell.getWidth() : 1;
        if (width === 0) {
          // Trailing half of a wide CJK / Hangul / emoji glyph.
          continue;
        }
        const ch = cell?.getChars() || " ";
        text += ch;
        cells.push(ch.length, width);

        let fg = defaultFg;
        let bg = defaultBg;
        if (cell) {
          if (cell.isFgRGB()) fg = (0xff000000 | cell.getFgColor()) >>> 0;
          else if (cell.isFgPalette()) fg = ansi256ToArgb(cell.getFgColor());

          if (cell.isBgRGB()) bg = (0xff000000 | cell.getBgColor()) >>> 0;
          else if (cell.isBgPalette()) bg = ansi256ToArgb(cell.getBgColor());

          if (cell.isInverse()) {
            const tmp = fg;
            fg = bg;
            bg = tmp;
          }
        }

        let flags = 0;
        if (cell?.isBold()) flags |= TERMINAL_STYLE_FLAGS.bold;
        if (cell?.isItalic()) flags |= TERMINAL_STYLE_FLAGS.italic;
        if (cell?.isUnderline()) flags |= TERMINAL_STYLE_FLAGS.underline;
        if (cell?.isStrikethrough()) flags |= TERMINAL_STYLE_FLAGS.strikethrough;
        if (cell?.isDim()) flags |= TERMINAL_STYLE_FLAGS.dim;

        if (
          spanCols > 0 &&
          fg === spanFg &&
          bg === spanBg &&
          flags === spanFlags
        ) {
          spanCols += width;
        } else {
          if (spanCols > 0) {
            styles.push(spanCols, spanFg, spanBg, spanFlags);
          }
          spanCols = width;
          spanFg = fg;
          spanBg = bg;
          spanFlags = flags;
        }
      }
      if (spanCols > 0) {
        styles.push(spanCols, spanFg, spanBg, spanFlags);
      }
      rows.push({ text, cells, styles });
    }

    return {
      columns: cols,
      rows,
      cursor: {
        row: buf.cursorY,
        col: buf.cursorX,
      },
    };
  }

  plainTextLines(): string[] {
    const buf = this.term.buffer.active;
    const out: string[] = [];
    for (let y = 0; y < this.term.rows; y++) {
      out.push(buf.getLine(buf.viewportY + y)?.translateToString(true) ?? "");
    }
    return out;
  }

  dispose(): void {
    this.term.dispose();
  }
}

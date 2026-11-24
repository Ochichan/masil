/**
 * Native cell-grid terminal view contract for `masil-terminal`.
 * JS owns xterm buffer state (`@xterm/headless` + unicode graphemes); native
 * iOS CoreText and Android Canvas paint fixed-column shaped cells.
 */
export interface MasilTerminalRow {
  text: string;
  /** Flattened `[utf16Length, columnSpan]` per cell. */
  cells: number[];
  /** Flattened `[columnCount, fgArgb, bgArgb, flags]` per style span. */
  styles: number[];
}

export interface MasilTerminalCursor {
  row: number;
  col: number;
}

export interface MasilTerminalFrame {
  columns: number;
  rows: MasilTerminalRow[];
  cursor: MasilTerminalCursor | null;
}

export interface MasilTerminalTheme {
  background: number;
  foreground: number;
  cursor: number;
}

export const TERMINAL_STYLE_FLAGS = {
  bold: 1,
  italic: 2,
  underline: 4,
  strikethrough: 8,
  dim: 16,
} as const;

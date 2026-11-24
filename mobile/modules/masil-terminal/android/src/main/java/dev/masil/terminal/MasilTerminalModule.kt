package dev.masil.terminal

import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition

class MasilTerminalModule : Module() {
  override fun definition() = ModuleDefinition {
    Name("MasilTerminal")

    View(MasilTerminalCanvasView::class) {
      Events("onGridMetrics")

      Prop("fontSize") { view: MasilTerminalCanvasView, size: Float ->
        view.updateFontSize(size)
      }

      Prop("theme") { view: MasilTerminalCanvasView, theme: Map<String, Any?> ->
        val bg = (theme["background"] as? Number)?.toLong()?.toInt() ?: 0xFF0C1016.toInt()
        val fg = (theme["foreground"] as? Number)?.toLong()?.toInt() ?: 0xFFE6EBF1.toInt()
        val cursor = (theme["cursor"] as? Number)?.toLong()?.toInt() ?: 0xFF5BB5C2.toInt()
        view.updateTheme(bg, fg, cursor)
      }

      Prop("frame") { view: MasilTerminalCanvasView, frame: Map<String, Any?> ->
        val columns = (frame["columns"] as? Number)?.toInt() ?: 80
        @Suppress("UNCHECKED_CAST")
        val rows = (frame["rows"] as? List<String>) ?: emptyList()
        @Suppress("UNCHECKED_CAST")
        val cur = frame["cursor"] as? Map<String, Any?>
        val cursorRow = (cur?.get("row") as? Number)?.toInt()
        val cursorCol = (cur?.get("col") as? Number)?.toInt()
        view.updateFrame(columns, rows, cursorRow, cursorCol)
      }
    }
  }
}

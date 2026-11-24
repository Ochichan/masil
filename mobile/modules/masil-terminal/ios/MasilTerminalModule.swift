import ExpoModulesCore

public class MasilTerminalModule: Module {
  public func definition() -> ModuleDefinition {
    Name("MasilTerminal")

    View(MasilTerminalCanvasView.self) {
      Events("onGridMetrics")

      Prop("fontSize") { (view: MasilTerminalCanvasView, size: CGFloat) in
        view.updateFontSize(size)
      }

      Prop("theme") { (view: MasilTerminalCanvasView, theme: [String: Any]) in
        let bg = (theme["background"] as? NSNumber)?.uint32Value ?? 0xFF0C1016
        let fg = (theme["foreground"] as? NSNumber)?.uint32Value ?? 0xFFE6EBF1
        let cursor = (theme["cursor"] as? NSNumber)?.uint32Value ?? 0xFF5BB5C2
        view.updateTheme(bg: bg, fg: fg, cursor: cursor)
      }

      Prop("frame") { (view: MasilTerminalCanvasView, frame: [String: Any]) in
        let columns = (frame["columns"] as? Int) ?? 80
        let rows = (frame["rows"] as? [String]) ?? []
        var cursorRow: Int? = nil
        var cursorCol: Int? = nil
        if let cur = frame["cursor"] as? [String: Any] {
          cursorRow = cur["row"] as? Int
          cursorCol = cur["col"] as? Int
        }
        view.updateFrame(columns: columns, rows: rows, cursorRow: cursorRow, cursorCol: cursorCol)
      }
    }
  }
}

import CoreText
import ExpoModulesCore
import UIKit

private struct ParsedRow {
  let raw: String
  let text: String
  let cells: [Int]
  let styles: [UInt32]
}

public class MasilTerminalCanvasView: ExpoView {
  let onGridMetrics = EventDispatcher()

  private var fontSize: CGFloat = 13.0
  private var cellWidth: CGFloat = 7.8
  private var cellHeight: CGFloat = 17.5
  private var baselineOffset: CGFloat = 13.0
  private var bgColor: UIColor = UIColor(red: 12 / 255, green: 16 / 255, blue: 22 / 255, alpha: 1)
  private var fgColor: UIColor = UIColor(red: 230 / 255, green: 235 / 255, blue: 241 / 255, alpha: 1)
  private var cursorColor: UIColor = UIColor(red: 91 / 255, green: 181 / 255, blue: 194 / 255, alpha: 1)
  private var columns: Int = 80
  private var parsedRows: [ParsedRow] = []
  private var rowCache: [String: ParsedRow] = [:]
  private var cursorRow: Int? = nil
  private var cursorCol: Int? = nil

  required init(appContext: AppContext? = nil) {
    super.init(appContext: appContext)
    isOpaque = true
    contentMode = .redraw
    recomputeMetrics()
  }

  func updateFontSize(_ size: CGFloat) {
    let clamped = max(8.0, min(28.0, size))
    if abs(clamped - fontSize) < 0.05 { return }
    fontSize = clamped
    recomputeMetrics()
    setNeedsDisplay()
  }

  func updateTheme(bg: UInt32, fg: UInt32, cursor: UInt32) {
    bgColor = argbColor(bg)
    fgColor = argbColor(fg)
    cursorColor = argbColor(cursor)
    backgroundColor = bgColor
    setNeedsDisplay()
  }

  func updateFrame(columns: Int, rows: [String], cursorRow: Int?, cursorCol: Int?) {
    self.columns = max(1, columns)
    self.cursorRow = cursorRow
    self.cursorCol = cursorCol
    var nextCache: [String: ParsedRow] = [:]
    var nextRows: [ParsedRow] = []
    nextRows.reserveCapacity(rows.count)
    for raw in rows {
      if let cached = rowCache[raw] {
        nextRows.append(cached)
        nextCache[raw] = cached
      } else {
        let parsed = parseEncodedRow(raw)
        nextRows.append(parsed)
        nextCache[raw] = parsed
      }
    }
    rowCache = nextCache
    parsedRows = nextRows
    setNeedsDisplay()
  }

  private func recomputeMetrics() {
    let font = CTFontCreateWithName("Menlo-Regular" as CFString, fontSize, nil)
    var glyph = CGGlyph(0)
    var uni: UniChar = 0x4D // 'M'
    CTFontGetGlyphsForCharacters(font, &uni, &glyph, 1)
    var adv = CGSize.zero
    CTFontGetAdvancesForGlyphs(font, .horizontal, &glyph, &adv, 1)
    let ascent = CTFontGetAscent(font)
    let descent = CTFontGetDescent(font)
    let leading = CTFontGetLeading(font)
    cellWidth = max(4.0, adv.width)
    cellHeight = ceil(ascent + descent + leading + 2.0)
    baselineOffset = ceil(ascent + 1.0)
    onGridMetrics([
      "cellWidth": cellWidth,
      "cellHeight": cellHeight,
    ])
  }

  public override func draw(_ rect: CGRect) {
    guard let ctx = UIGraphicsGetCurrentContext() else { return }
    ctx.setFillColor(bgColor.cgColor)
    ctx.fill(rect)

    let regularFont = CTFontCreateWithName("Menlo-Regular" as CFString, fontSize, nil)
    let boldFont = CTFontCreateWithName("Menlo-Bold" as CFString, fontSize, nil)

    for (rowIdx, row) in parsedRows.enumerated() {
      let yTop = CGFloat(rowIdx) * cellHeight
      if yTop > rect.maxY + cellHeight || yTop + cellHeight < rect.minY - cellHeight {
        continue
      }

      // Paint background spans per style run.
      var colCursor = 0
      var sIdx = 0
      while sIdx + 3 < row.styles.count {
        let spanCols = Int(row.styles[sIdx])
        let bgArgb = row.styles[sIdx + 2]
        if (bgArgb >> 24) > 0 && argbColor(bgArgb) != bgColor {
          ctx.setFillColor(argbColor(bgArgb).cgColor)
          ctx.fill(
            CGRect(
              x: CGFloat(colCursor) * cellWidth,
              y: yTop,
              width: CGFloat(spanCols) * cellWidth,
              height: cellHeight
            )
          )
        }
        colCursor += spanCols
        sIdx += 4
      }

      // Paint fixed-column cells via CoreText with explicit LTR base direction.
      let nsText = row.text as NSString
      var utf16Pos = 0
      var cellCol = 0
      var stylePos = 0
      var styleRem = row.styles.count >= 4 ? Int(row.styles[0]) : columns
      var curFg: UInt32 = row.styles.count >= 4 ? row.styles[1] : 0xFFE6EBF1
      var curFlags: UInt32 = row.styles.count >= 4 ? row.styles[3] : 0

      var cIdx = 0
      while cIdx + 1 < row.cells.count {
        let utf16Len = row.cells[cIdx]
        let colSpan = row.cells[cIdx + 1]
        cIdx += 2
        if utf16Pos + utf16Len > nsText.length { break }
        let slice = nsText.substring(with: NSRange(location: utf16Pos, length: utf16Len))
        utf16Pos += utf16Len

        if slice != " " && !slice.isEmpty {
          let font = (curFlags & 1) != 0 ? boldFont : regularFont
          let fg = (curFlags & 16) != 0 ? argbColor(curFg).withAlphaComponent(0.6) : argbColor(curFg)
          let attrs: [NSAttributedString.Key: Any] = [
            NSAttributedString.Key(kCTFontAttributeName as String): font,
            NSAttributedString.Key(kCTForegroundColorAttributeName as String): fg.cgColor,
          ]
          let line = CTLineCreateWithAttributedString(NSAttributedString(string: slice, attributes: attrs))
          ctx.saveGState()
          ctx.textMatrix = CGAffineTransform(scaleX: 1.0, y: -1.0)
          ctx.textPosition = CGPoint(x: CGFloat(cellCol) * cellWidth, y: yTop + baselineOffset)
          CTLineDraw(line, ctx)
          ctx.restoreGState()
        }

        cellCol += colSpan
        styleRem -= colSpan
        while styleRem <= 0 && stylePos + 7 < row.styles.count {
          stylePos += 4
          styleRem += Int(row.styles[stylePos])
          curFg = row.styles[stylePos + 1]
          curFlags = row.styles[stylePos + 3]
        }
      }
    }

    if let cr = cursorRow, let cc = cursorCol {
      ctx.setFillColor(cursorColor.withAlphaComponent(0.75).cgColor)
      ctx.fill(
        CGRect(
          x: CGFloat(cc) * cellWidth,
          y: CGFloat(cr) * cellHeight,
          width: cellWidth,
          height: cellHeight
        )
      )
    }
  }

  private func parseEncodedRow(_ raw: String) -> ParsedRow {
    guard let firstSemi = raw.firstIndex(of: ";") else {
      return ParsedRow(raw: raw, text: raw, cells: [], styles: [])
    }
    let afterFirst = raw.index(after: firstSemi)
    guard let secondSemi = raw[afterFirst...].firstIndex(of: ";") else {
      return ParsedRow(raw: raw, text: raw, cells: [], styles: [])
    }
    let cellsStr = raw[..<firstSemi]
    let stylesStr = raw[afterFirst..<secondSemi]
    let text = String(raw[raw.index(after: secondSemi)...])
    let cells = cellsStr.split(separator: ",").compactMap { Int($0) }
    let styles = stylesStr.split(separator: ",").compactMap { UInt32($0) }
    return ParsedRow(raw: raw, text: text, cells: cells, styles: styles)
  }

  private func argbColor(_ argb: UInt32) -> UIColor {
    let a = CGFloat((argb >> 24) & 0xFF) / 255.0
    let r = CGFloat((argb >> 16) & 0xFF) / 255.0
    let g = CGFloat((argb >> 8) & 0xFF) / 255.0
    let b = CGFloat(argb & 0xFF) / 255.0
    return UIColor(red: r, green: g, blue: b, alpha: a)
  }
}

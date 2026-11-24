package dev.masil.terminal

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.icu.text.BreakIterator
import expo.modules.kotlin.AppContext
import expo.modules.kotlin.viewevent.EventDispatcher
import expo.modules.kotlin.views.ExpoView
import kotlin.math.ceil

private data class ParsedAndroidRow(
  val raw: String,
  val text: String,
  val cells: IntArray,
  val styles: LongArray,
)

class MasilTerminalCanvasView(
  context: Context,
  appContext: AppContext,
) : ExpoView(context, appContext) {
  private val onGridMetrics by EventDispatcher()

  private var fontSizeSp = 13f
  private var cellWidthPx = 18f
  private var cellHeightPx = 42f
  private var baselinePx = 32f
  private var bgColor: Int = 0xFF0C1016.toInt()
  private var fgColor: Int = 0xFFE6EBF1.toInt()
  private var cursorColor: Int = 0xFF5BB5C2.toInt()
  private var columns: Int = 80
  private var rows: List<ParsedAndroidRow> = emptyList()
  private val rowCache = HashMap<String, ParsedAndroidRow>()
  private var cursorRow: Int? = null
  private var cursorCol: Int? = null

  private val textPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
    typeface = Typeface.MONOSPACE
  }
  private val bgPaint = Paint()

  init {
    setWillNotDraw(false)
    recomputeMetrics()
  }

  fun updateFontSize(size: Float) {
    val clamped = size.coerceIn(8f, 28f)
    if (kotlin.math.abs(clamped - fontSizeSp) < 0.05f) return
    fontSizeSp = clamped
    recomputeMetrics()
    invalidate()
  }

  fun updateTheme(bg: Int, fg: Int, cursor: Int) {
    bgColor = bg
    fgColor = fg
    cursorColor = cursor
    invalidate()
  }

  fun updateFrame(columns: Int, rawRows: List<String>, cursorRow: Int?, cursorCol: Int?) {
    this.columns = columns.coerceAtLeast(1)
    this.cursorRow = cursorRow
    this.cursorCol = cursorCol
    val nextCache = HashMap<String, ParsedAndroidRow>(rawRows.size)
    val parsed = ArrayList<ParsedAndroidRow>(rawRows.size)
    for (raw in rawRows) {
      val item = rowCache[raw] ?: parseEncodedRow(raw)
      nextCache[raw] = item
      parsed.add(item)
    }
    rowCache.clear()
    rowCache.putAll(nextCache)
    rows = parsed
    invalidate()
  }

  private fun recomputeMetrics() {
    val density = resources.displayMetrics.scaledDensity
    textPaint.textSize = fontSizeSp * density
    val fm = textPaint.fontMetrics
    cellWidthPx = textPaint.measureText("M").coerceAtLeast(4f)
    cellHeightPx = ceil((fm.descent - fm.ascent + fm.leading + 2f).toDouble()).toFloat()
    baselinePx = ceil((-fm.ascent + 1f).toDouble()).toFloat()
    onGridMetrics(
      mapOf(
        "cellWidth" to (cellWidthPx / resources.displayMetrics.density),
        "cellHeight" to (cellHeightPx / resources.displayMetrics.density),
      )
    )
  }

  override fun onDraw(canvas: Canvas) {
    super.onDraw(canvas)
    canvas.drawColor(bgColor)

    for (rowIdx in rows.indices) {
      val row = rows[rowIdx]
      val yTop = rowIdx * cellHeightPx

      // Paint background spans.
      var colCursor = 0
      var sIdx = 0
      while (sIdx + 3 < row.styles.size) {
        val spanCols = row.styles[sIdx].toInt()
        val spanBg = row.styles[sIdx + 2].toInt()
        if (((spanBg ushr 24) and 0xFF) > 0 && spanBg != bgColor) {
          bgPaint.color = spanBg
          canvas.drawRect(
            colCursor * cellWidthPx,
            yTop,
            (colCursor + spanCols) * cellWidthPx,
            yTop + cellHeightPx,
            bgPaint,
          )
        }
        colCursor += spanCols
        sIdx += 4
      }

      // Draw shaped cells anchored to xterm's authoritative columns.
      val text = row.text
      var utf16Pos = 0
      var cellCol = 0
      var stylePos = 0
      var styleRem = if (row.styles.size >= 4) row.styles[0].toInt() else columns
      var curFg = if (row.styles.size >= 4) row.styles[1].toInt() else fgColor
      var curFlags = if (row.styles.size >= 4) row.styles[3].toInt() else 0

      // Respect ICU grapheme cluster boundaries when measuring runs.
      val bi = BreakIterator.getCharacterInstance()
      bi.setText(text)

      var cIdx = 0
      while (cIdx + 1 < row.cells.size) {
        val utf16Len = row.cells[cIdx]
        val colSpan = row.cells[cIdx + 1]
        cIdx += 2
        if (utf16Pos + utf16Len > text.length) break

        val start = utf16Pos
        val end = utf16Pos + utf16Len
        utf16Pos = end

        if (end > start && !(end - start == 1 && text[start] == ' ')) {
          textPaint.color = curFg
          textPaint.isFakeBoldText = (curFlags and 1) != 0
          textPaint.alpha = if ((curFlags and 16) != 0) 150 else 255
          canvas.drawTextRun(
            text,
            start,
            end,
            0,
            text.length,
            cellCol * cellWidthPx,
            yTop + baselinePx,
            false,
            textPaint,
          )
        }

        cellCol += colSpan
        styleRem -= colSpan
        while (styleRem <= 0 && stylePos + 7 < row.styles.size) {
          stylePos += 4
          styleRem += row.styles[stylePos].toInt()
          curFg = row.styles[stylePos + 1].toInt()
          curFlags = row.styles[stylePos + 3].toInt()
        }
      }
    }

    val cr = cursorRow
    val cc = cursorCol
    if (cr != null && cc != null) {
      bgPaint.color = (cursorColor and 0x00FFFFFF) or (0xBF shl 24)
      canvas.drawRect(
        cc * cellWidthPx,
        cr * cellHeightPx,
        (cc + 1) * cellWidthPx,
        (cr + 1) * cellHeightPx,
        bgPaint,
      )
    }
  }

  private fun parseEncodedRow(raw: String): ParsedAndroidRow {
    val first = raw.indexOf(';')
    if (first < 0) return ParsedAndroidRow(raw, raw, IntArray(0), LongArray(0))
    val second = raw.indexOf(';', first + 1)
    if (second < 0) return ParsedAndroidRow(raw, raw, IntArray(0), LongArray(0))
    val cellsStr = raw.substring(0, first)
    val stylesStr = raw.substring(first + 1, second)
    val text = raw.substring(second + 1)
    val cells = if (cellsStr.isEmpty()) IntArray(0) else cellsStr.split(',').mapNotNull { it.toIntOrNull() }.toIntArray()
    val styles = if (stylesStr.isEmpty()) LongArray(0) else stylesStr.split(',').mapNotNull { it.toLongOrNull() }.toLongArray()
    return ParsedAndroidRow(raw, text, cells, styles)
  }
}

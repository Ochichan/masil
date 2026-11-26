package dev.masil.livepulse

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition

class MasilLivePulseModule : Module() {
  companion object {
    const val CHANNEL_ID = "masil-fleet-pulse"
    const val NOTIFICATION_ID_BASE = 42000
  }

  override fun definition() = ModuleDefinition {
    Name("MasilLivePulse")

    Events("onPushToken")

    Function("isSupported") {
      true
    }

    AsyncFunction("observePushToStartToken") { _: Boolean ->
      mapOf("observing" to false)
    }

    AsyncFunction("startOrUpdatePulse") { payload: Map<String, Any?> ->
      val ctx = appContext.reactContext ?: return@AsyncFunction mapOf("active" to false)
      val enabled = (payload["enabled"] as? Boolean) ?: false
      val hostId = (payload["hostId"] as? String) ?: ""
      if (!enabled || hostId.isEmpty()) {
        return@AsyncFunction mapOf("active" to false, "reason" to "opt_in_disabled")
      }
      val hostName = (payload["hostName"] as? String) ?: "masil"
      val working = (payload["workingCount"] as? Number)?.toInt() ?: 0
      val blocked = (payload["blockedCount"] as? Number)?.toInt() ?: 0
      val done = (payload["doneCount"] as? Number)?.toInt() ?: 0
      val alerts = (payload["alertCount"] as? Number)?.toInt() ?: 0
      val topAlias = (payload["topAgentAlias"] as? String)?.take(32)
      val topEvent = (payload["topEventKind"] as? String)
      val topSummary = (payload["topSummaryLine"] as? String)?.take(60)
      val deepLink = (payload["deepLink"] as? String) ?: "masil://host/$hostId"

      ensureChannel(ctx)

      val openIntent = Intent(Intent.ACTION_VIEW, Uri.parse(deepLink)).apply {
        flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
      }
      val openPending = PendingIntent.getActivity(
        ctx,
        hostId.hashCode(),
        openIntent,
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
      )

      val inboxIntent = Intent(Intent.ACTION_VIEW, Uri.parse("masil://host/$hostId/inbox")).apply {
        flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
      }
      val inboxPending = PendingIntent.getActivity(
        ctx,
        hostId.hashCode() + 1,
        inboxIntent,
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
      )

      val focusOffIntent = Intent(Intent.ACTION_VIEW, Uri.parse("masil://host/$hostId/focus/off")).apply {
        flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
      }
      val focusOffPending = PendingIntent.getActivity(
        ctx,
        hostId.hashCode() + 2,
        focusOffIntent,
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
      )

      val countsLine = buildString {
        append("⚡ $working  🟠 $blocked  ✓ $done")
        if (alerts > 0) append("  🚨 $alerts")
      }
      val bodyLine = when {
        !topSummary.isNullOrBlank() && !topAlias.isNullOrBlank() -> "$topAlias · $topSummary"
        !topAlias.isNullOrBlank() && !topEvent.isNullOrBlank() -> "$topAlias · ${formatEventLabel(topEvent)}"
        else -> countsLine
      }

      val builder = NotificationCompat.Builder(ctx, CHANNEL_ID)
        .setSmallIcon(android.R.drawable.ic_dialog_info)
        .setContentTitle("masil · $hostName · $countsLine")
        .setContentText(bodyLine)
        .setOngoing(true)
        .setOnlyAlertOnce(true)
        .setSilent(true)
        .setPriority(NotificationCompat.PRIORITY_LOW)
        .setContentIntent(openPending)
        .addAction(0, "Open Inbox", inboxPending)
        .addAction(0, "End Focus", focusOffPending)

      try {
        NotificationManagerCompat.from(ctx).notify(notificationId(hostId), builder.build())
        mapOf("active" to true)
      } catch (e: SecurityException) {
        mapOf("active" to false, "error" to "permission_denied")
      }
    }

    AsyncFunction("endPulse") { hostId: String? ->
      val ctx = appContext.reactContext ?: return@AsyncFunction
      val nm = NotificationManagerCompat.from(ctx)
      if (hostId.isNullOrBlank()) {
        nm.cancelAll()
      } else {
        nm.cancel(notificationId(hostId))
      }
    }
  }

  private fun notificationId(hostId: String): Int {
    return NOTIFICATION_ID_BASE + (hostId.hashCode() and 0x0FFF)
  }

  private fun formatEventLabel(kind: String): String = when (kind) {
    "approval_requested" -> "Approval requested"
    "question_asked" -> "Question waiting"
    "loop_paused" -> "Loop paused"
    "hard_conflict" -> "Worktree conflict"
    "bs_flagged" -> "Audit warning"
    "turn_finished" -> "Turn finished"
    else -> "Attention needed"
  }

  private fun ensureChannel(ctx: Context) {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      val nm = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
      if (nm.getNotificationChannel(CHANNEL_ID) == null) {
        val ch = NotificationChannel(
          CHANNEL_ID,
          "Masil Fleet Pulse",
          NotificationManager.IMPORTANCE_LOW
        ).apply {
          description = "Silent ongoing fleet status bar for active Masil agents"
          setSound(null, null)
          enableVibration(false)
        }
        nm.createNotificationChannel(ch)
      }
    }
  }
}

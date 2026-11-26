import ActivityKit
import SwiftUI
import WidgetKit

@available(iOS 16.2, *)
private func eventLabel(_ kind: String?) -> String {
  switch kind {
  case "approval_requested": return "Approval requested"
  case "question_asked": return "Question waiting"
  case "loop_paused": return "Loop paused"
  case "hard_conflict": return "Worktree conflict"
  case "bs_flagged": return "Audit warning"
  case "turn_finished": return "Turn finished"
  default: return "Fleet active"
  }
}

@available(iOS 16.2, *)
public struct MasilLiveActivityWidget: Widget {
  public init() {}

  public var body: some WidgetConfiguration {
    ActivityConfiguration(for: MasilFleetAttributes.self) { context in
      // Lock Screen / Notification Banner
      VStack(alignment: .leading, spacing: 8) {
        HStack {
          Text("masil · \(context.attributes.hostName)")
            .font(.caption.weight(.bold))
            .foregroundStyle(.secondary)
          Spacer()
          HStack(spacing: 8) {
            Label("\(context.state.workingCount)", systemImage: "bolt.fill")
              .foregroundStyle(.green)
            Label("\(context.state.blockedCount)", systemImage: "exclamationmark.circle.fill")
              .foregroundStyle(.orange)
            Label("\(context.state.doneCount)", systemImage: "checkmark.circle.fill")
              .foregroundStyle(.secondary)
            if context.state.alertCount > 0 {
              Label("\(context.state.alertCount)", systemImage: "shield.lefthalf.filled")
                .foregroundStyle(.red)
            }
          }
          .font(.caption.monospacedDigit().weight(.semibold))
        }

        if let alias = context.state.topAgentAlias, !alias.isEmpty {
          HStack(spacing: 6) {
            Text(alias)
              .font(.subheadline.weight(.bold))
              .lineLimit(1)
            Text("· \(eventLabel(context.state.topEventKind))")
              .font(.subheadline)
              .foregroundStyle(.secondary)
              .lineLimit(1)
          }
        }

        if let summary = context.state.topSummaryLine, !summary.isEmpty {
          Text(summary)
            .font(.caption)
            .foregroundStyle(.secondary)
            .lineLimit(1)
        }
      }
      .padding(14)
      .widgetURL(URL(string: context.state.deepLink))
      .activityBackgroundTint(Color(red: 0.08, green: 0.11, blue: 0.15))
      .activitySystemActionForegroundColor(Color(red: 0.36, green: 0.71, blue: 0.76))
    } dynamicIsland: { context in
      DynamicIsland {
        DynamicIslandExpandedRegion(.leading) {
          Text(context.attributes.hostName)
            .font(.caption.weight(.bold))
            .lineLimit(1)
        }
        DynamicIslandExpandedRegion(.trailing) {
          HStack(spacing: 6) {
            Text("⚡\(context.state.workingCount)")
            if context.state.blockedCount > 0 {
              Text("🟠\(context.state.blockedCount)")
            }
            if context.state.alertCount > 0 {
              Text("🚨\(context.state.alertCount)")
            }
          }
          .font(.caption.monospacedDigit().weight(.semibold))
        }
        DynamicIslandExpandedRegion(.bottom) {
          VStack(alignment: .leading, spacing: 3) {
            if let alias = context.state.topAgentAlias, !alias.isEmpty {
              Text("\(alias) · \(eventLabel(context.state.topEventKind))")
                .font(.caption.weight(.semibold))
                .lineLimit(1)
            }
            if let summary = context.state.topSummaryLine, !summary.isEmpty {
              Text(summary)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .lineLimit(1)
            }
          }
          .frame(maxWidth: .infinity, alignment: .leading)
        }
      } compactLeading: {
        Text("⚡\(context.state.workingCount)")
          .font(.caption2.monospacedDigit().weight(.bold))
          .foregroundStyle(.green)
      } compactTrailing: {
        if context.state.alertCount > 0 {
          Text("🚨\(context.state.alertCount)")
            .font(.caption2.monospacedDigit().weight(.bold))
            .foregroundStyle(.red)
        } else {
          Text("🟠\(context.state.blockedCount)")
            .font(.caption2.monospacedDigit().weight(.bold))
            .foregroundStyle(context.state.blockedCount > 0 ? .orange : .secondary)
        }
      } minimal: {
        if context.state.blockedCount > 0 {
          Text("🟠\(context.state.blockedCount)")
            .font(.caption2.monospacedDigit().weight(.bold))
        } else {
          Text("⚡\(context.state.workingCount)")
            .font(.caption2.monospacedDigit().weight(.bold))
        }
      }
      .widgetURL(URL(string: context.state.deepLink))
    }
  }
}

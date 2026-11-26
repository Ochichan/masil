import Foundation
#if canImport(ActivityKit)
import ActivityKit

/// Shared ActivityKit attributes for both the main app and `MasilLiveActivityExtension`.
/// Enforces D-09 minimal metadata privacy: only counts, bounded alias (<=32 chars),
/// fixed event kind, optional opt-in redacted summary (<=60 chars), and deep link.
@available(iOS 16.2, *)
public struct MasilFleetAttributes: ActivityAttributes {
  public struct ContentState: Codable, Hashable {
    public var workingCount: Int
    public var blockedCount: Int
    public var doneCount: Int
    public var alertCount: Int
    public var topAgentAlias: String?
    public var topEventKind: String?
    public var topSummaryLine: String?
    public var deepLink: String
    public var updatedAt: Double

    public init(
      workingCount: Int,
      blockedCount: Int,
      doneCount: Int,
      alertCount: Int,
      topAgentAlias: String?,
      topEventKind: String?,
      topSummaryLine: String?,
      deepLink: String,
      updatedAt: Double
    ) {
      self.workingCount = workingCount
      self.blockedCount = blockedCount
      self.doneCount = doneCount
      self.alertCount = alertCount
      self.topAgentAlias = topAgentAlias
      self.topEventKind = topEventKind
      self.topSummaryLine = topSummaryLine
      self.deepLink = deepLink
      self.updatedAt = updatedAt
    }
  }

  public var hostId: String
  public var hostName: String

  public init(hostId: String, hostName: String) {
    self.hostId = hostId
    self.hostName = hostName
  }
}
#endif

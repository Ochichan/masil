import ExpoModulesCore
import Foundation
#if canImport(ActivityKit)
import ActivityKit
#endif

public class MasilLivePulseModule: Module {
  private var tokenTasks: [String: Task<Void, Never>] = [:]

  public func definition() -> ModuleDefinition {
    Name("MasilLivePulse")

    Events("onPushToken")

    OnDestroy {
      for (_, task) in self.tokenTasks {
        task.cancel()
      }
      self.tokenTasks.removeAll()
    }

    Function("isSupported") { () -> Bool in
      #if canImport(ActivityKit)
      if #available(iOS 16.2, *) {
        return ActivityAuthorizationInfo().areActivitiesEnabled
      }
      #endif
      return false
    }

    AsyncFunction("observePushToStartToken") { (enabled: Bool) -> [String: Any] in
      if !enabled {
        self.tokenTasks["push_to_start"]?.cancel()
        self.tokenTasks.removeValue(forKey: "push_to_start")
        return ["observing": false]
      }
      #if canImport(ActivityKit)
      if #available(iOS 17.2, *) {
        self.tokenTasks["push_to_start"]?.cancel()
        self.tokenTasks["push_to_start"] = Task { [weak self] in
          for await tokenData in Activity<MasilFleetAttributes>.pushToStartTokenUpdates {
            let hex = tokenData.map { String(format: "%02x", $0) }.joined()
            self?.sendEvent("onPushToken", [
              "kind": "push_to_start",
              "token": hex,
            ])
          }
        }
        return ["observing": true]
      }
      #endif
      return ["observing": false]
    }

    AsyncFunction("startOrUpdatePulse") { (payload: [String: Any]) -> [String: Any] in
      let enabled = (payload["enabled"] as? Bool) ?? false
      let hostId = (payload["hostId"] as? String) ?? ""
      let hostName = (payload["hostName"] as? String) ?? "masil"
      if !enabled || hostId.isEmpty {
        return ["active": false, "reason": "opt_in_disabled"]
      }

      #if canImport(ActivityKit)
      if #available(iOS 16.2, *) {
        guard ActivityAuthorizationInfo().areActivitiesEnabled else {
          return ["active": false, "reason": "activities_disabled"]
        }
        let state = MasilFleetAttributes.ContentState(
          workingCount: (payload["workingCount"] as? Int) ?? 0,
          blockedCount: (payload["blockedCount"] as? Int) ?? 0,
          doneCount: (payload["doneCount"] as? Int) ?? 0,
          alertCount: (payload["alertCount"] as? Int) ?? 0,
          topAgentAlias: payload["topAgentAlias"] as? String,
          topEventKind: payload["topEventKind"] as? String,
          topSummaryLine: payload["topSummaryLine"] as? String,
          deepLink: (payload["deepLink"] as? String) ?? "masil://host/\(hostId)",
          updatedAt: (payload["updatedAt"] as? Double) ?? Date().timeIntervalSince1970
        )

        if let existing = Activity<MasilFleetAttributes>.activities.first(where: { $0.attributes.hostId == hostId }) {
          let content = ActivityContent(state: state, staleDate: Date().addingTimeInterval(3600))
          await existing.update(content)
          self.watchActivityPushToken(hostId: hostId, activity: existing)
          return ["active": true, "activityId": existing.id, "updated": true]
        }

        let attrs = MasilFleetAttributes(hostId: hostId, hostName: hostName)
        let content = ActivityContent(state: state, staleDate: Date().addingTimeInterval(3600))
        do {
          let activity = try Activity<MasilFleetAttributes>.request(
            attributes: attrs,
            content: content,
            pushType: .token
          )
          self.watchActivityPushToken(hostId: hostId, activity: activity)
          return ["active": true, "activityId": activity.id, "updated": false]
        } catch {
          return ["active": false, "error": error.localizedDescription]
        }
      }
      #endif
      return ["active": false, "reason": "ios_version_unsupported"]
    }

    AsyncFunction("endPulse") { (hostId: String?) in
      #if canImport(ActivityKit)
      if #available(iOS 16.2, *) {
        for activity in Activity<MasilFleetAttributes>.activities {
          if hostId == nil || hostId?.isEmpty == true || activity.attributes.hostId == hostId {
            let finalContent = ActivityContent(state: activity.content.state, staleDate: nil)
            await activity.end(finalContent, dismissalPolicy: .immediate)
            self.tokenTasks["activity:\(activity.attributes.hostId)"]?.cancel()
            self.tokenTasks.removeValue(forKey: "activity:\(activity.attributes.hostId)")
          }
        }
      }
      #endif
    }
  }

  #if canImport(ActivityKit)
  @available(iOS 16.2, *)
  private func watchActivityPushToken(hostId: String, activity: Activity<MasilFleetAttributes>) {
    let key = "activity:\(hostId)"
    if self.tokenTasks[key] != nil {
      return
    }
    self.tokenTasks[key] = Task { [weak self] in
      for await tokenData in activity.pushTokenUpdates {
        let hex = tokenData.map { String(format: "%02x", $0) }.joined()
        self?.sendEvent("onPushToken", [
          "kind": "activity_update",
          "hostId": hostId,
          "token": hex,
        ])
      }
    }
  }
  #endif
}

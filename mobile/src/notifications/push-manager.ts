/**
 * Background push registration and deep-link routing (`masil://host/<id>/agent/<pane>`).
 * Registers native APNs / FCM device tokens with each paired host so the
 * optional `masil-relay` push bridge can wake the phone with minimal metadata.
 */
import * as Notifications from "expo-notifications";
import { Platform } from "react-native";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";

export async function registerPushTokenWithHost(hostId: string): Promise<boolean> {
  if (Platform.OS === "web") return false;
  try {
    const existing = await Notifications.getPermissionsAsync();
    let granted = existing.granted;
    if (!granted) {
      const asked = await Notifications.requestPermissionsAsync();
      granted = asked.granted;
    }
    if (!granted) return false;

    const devicePush = await Notifications.getDevicePushTokenAsync();
    const token = String(devicePush.data ?? "");
    if (!token) return false;

    const client = getHostClient(hostId);
    if (!client || client.connectionState !== "connected") return false;
    await client.request("host.push.register.request", {
      platform: Platform.OS === "ios" ? "ios" : "android",
      token,
    });
    return true;
  } catch {
    return false;
  }
}

export function subscribeNotificationDeepLinks(): () => void {
  const sub = Notifications.addNotificationResponseReceivedListener(
    (response) => {
      const data = response.notification.request.content.data as
        | { host_id?: string; agent_id?: string; deep_link?: string }
        | undefined;
      if (!data) return;
      const hostId = data.host_id;
      const paneId = data.agent_id;
      if (!hostId) return;

      const runtime = useHostRuntimeStore.getState();
      const host = runtime.hosts[hostId];
      if (!host) return;
      const match = paneId
        ? host.agents.find((a) => a.pane === paneId || a.remote_id === paneId)
        : host.agents[0];
      if (match) {
        runtime.selectAgent(hostId, match.remote_id);
      } else {
        runtime.selectHost(hostId);
      }
      useMobilePanelsStore.getState().selectPanel("agent");
    }
  );
  return () => sub.remove();
}

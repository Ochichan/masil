/**
 * Local & background push notification manager and deep-link routing
 * (`masil://host/<id>/agent/<pane>`).
 *
 * - Configures foreground presentation via `setNotificationHandler` and Android
 *   high-priority channels (`masil-attention`, `masil-completed`) so live alerts
 *   from direct LAN/Tailscale or relay WebSockets reliably ring & display.
 * - Registers native APNs / FCM device tokens with each paired host so the
 *   optional `masil-relay` push bridge can also wake the phone while suspended.
 */
import * as Notifications from "expo-notifications";
import { Platform } from "react-native";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import {
  type AgentNotificationEvent,
  type NotificationDeepLinkTarget,
  extractDeepLinkTarget,
  formatNotificationCopy,
} from "./agent-notifications";

export type NotificationPermissionState = "granted" | "denied" | "undetermined";

let handlerConfigured = false;
let channelsConfigured = false;
let pendingDeepLink: NotificationDeepLinkTarget | null = null;

function ensureNotificationHandler(): void {
  if (handlerConfigured || Platform.OS === "web") return;
  handlerConfigured = true;
  try {
    Notifications.setNotificationHandler({
      handleNotification: async () => ({
        shouldPlaySound: true,
        shouldSetBadge: true,
        shouldShowBanner: true,
        shouldShowList: true,
      }),
    });
  } catch {
    // Ignore on unsupported runtime.
  }
}

export async function ensureAndroidNotificationChannels(): Promise<void> {
  if (channelsConfigured || Platform.OS !== "android") return;
  channelsConfigured = true;
  try {
    await Notifications.setNotificationChannelAsync("masil-attention", {
      name: "입력 필요 · 승인 · 질문",
      description: "에이전트가 권한 승인, 질문 답변, 또는 사용자 입력을 기다릴 때 알립니다.",
      importance: Notifications.AndroidImportance.MAX,
      sound: "default",
      vibrationPattern: [0, 250, 180, 250],
      lightColor: "#ebb85c",
      enableVibrate: true,
      lockscreenVisibility: Notifications.AndroidNotificationVisibility.PUBLIC,
    });
    await Notifications.setNotificationChannelAsync("masil-completed", {
      name: "작업 완료",
      description: "에이전트가 작업을 마치고 대기 상태로 전환되었을 때 알립니다.",
      importance: Notifications.AndroidImportance.HIGH,
      sound: "default",
      vibrationPattern: [0, 180],
      lightColor: "#74c48a",
      enableVibrate: true,
      lockscreenVisibility: Notifications.AndroidNotificationVisibility.PUBLIC,
    });
  } catch {
    channelsConfigured = false;
  }
}

export async function setupNotificationSystem(): Promise<NotificationPermissionState> {
  if (Platform.OS === "web") return "denied";
  ensureNotificationHandler();
  await ensureAndroidNotificationChannels();
  return getNotificationPermissionStatus();
}

export async function getNotificationPermissionStatus(): Promise<NotificationPermissionState> {
  if (Platform.OS === "web") return "denied";
  try {
    const res = await Notifications.getPermissionsAsync();
    if (res.granted) return "granted";
    return res.status === "denied" ? "denied" : "undetermined";
  } catch {
    return "undetermined";
  }
}

export async function requestNotificationPermissions(): Promise<boolean> {
  if (Platform.OS === "web") return false;
  ensureNotificationHandler();
  await ensureAndroidNotificationChannels();
  try {
    const existing = await Notifications.getPermissionsAsync();
    if (existing.granted) return true;
    const asked = await Notifications.requestPermissionsAsync({
      ios: {
        allowAlert: true,
        allowBadge: true,
        allowSound: true,
      },
    });
    return asked.granted;
  } catch {
    return false;
  }
}

export async function deliverLocalAgentNotification(
  event: AgentNotificationEvent,
  language: string
): Promise<boolean> {
  if (Platform.OS === "web") return false;
  try {
    ensureNotificationHandler();
    await ensureAndroidNotificationChannels();
    const perm = await Notifications.getPermissionsAsync();
    if (!perm.granted) return false;

    const copy = formatNotificationCopy(event, language);
    const deepLink = `masil://host/${encodeURIComponent(event.hostId)}/agent/${encodeURIComponent(event.paneId || event.remoteId)}`;

    await Notifications.scheduleNotificationAsync({
      content: {
        title: copy.title,
        body: copy.body,
        sound: "default",
        priority:
          copy.channelId === "masil-attention"
            ? Notifications.AndroidNotificationPriority.MAX
            : Notifications.AndroidNotificationPriority.HIGH,
        data: {
          host_id: event.hostId,
          remote_id: event.remoteId,
          agent_id: event.paneId,
          event_kind: event.kind,
          deep_link: deepLink,
        },
      },
      trigger:
        Platform.OS === "android"
          ? { channelId: copy.channelId }
          : null,
    });
    return true;
  } catch {
    return false;
  }
}

export async function sendTestNotification(language: string): Promise<boolean> {
  if (Platform.OS === "web") return false;
  const granted = await requestNotificationPermissions();
  if (!granted) return false;
  const isKo = language.toLowerCase().startsWith("ko");
  try {
    await Notifications.scheduleNotificationAsync({
      content: {
        title: isKo ? "masil 알림 테스트" : "masil notification test",
        body: isKo
          ? "에이전트 작업 완료 또는 입력 필요 시 이처럼 알림이 표시됩니다."
          : "Agent completions and input-needed prompts will notify you like this.",
        sound: "default",
        data: { test: true },
      },
      trigger:
        Platform.OS === "android"
          ? { channelId: "masil-attention" }
          : null,
    });
    return true;
  } catch {
    return false;
  }
}

export async function updateAppBadgeCount(count: number): Promise<void> {
  if (Platform.OS === "web") return;
  try {
    await Notifications.setBadgeCountAsync(Math.max(0, count));
  } catch {
    // Ignore on platforms/launchers that do not support badge counts.
  }
}

export async function registerPushTokenWithHost(
  hostId: string,
  options?: { promptIfUndetermined?: boolean }
): Promise<{ ok: boolean; reason?: string }> {
  if (Platform.OS === "web") return { ok: false, reason: "web_unsupported" };
  try {
    ensureNotificationHandler();
    await ensureAndroidNotificationChannels();
    const existing = await Notifications.getPermissionsAsync();
    let granted = existing.granted;
    if (!granted && options?.promptIfUndetermined !== false) {
      const asked = await Notifications.requestPermissionsAsync();
      granted = asked.granted;
    }
    if (!granted) return { ok: false, reason: "permission_denied" };

    const devicePush = await Notifications.getDevicePushTokenAsync();
    const token = String(devicePush.data ?? "");
    if (!token) return { ok: false, reason: "no_device_token" };

    const client = getHostClient(hostId);
    if (!client || client.connectionState !== "connected") {
      return { ok: false, reason: "host_disconnected" };
    }
    await client.request("host.push.register.request", {
      platform: Platform.OS === "ios" ? "ios" : "android",
      token,
    });
    return { ok: true };
  } catch (err) {
    return {
      ok: false,
      reason: err instanceof Error ? err.message : "push_token_unavailable",
    };
  }
}

function applyDeepLinkTarget(target: NotificationDeepLinkTarget): boolean {
  const runtime = useHostRuntimeStore.getState();
  const host = runtime.hosts[target.hostId];
  if (!host) return false;
  const match =
    (target.remoteId
      ? host.agents.find((a) => a.remote_id === target.remoteId)
      : undefined) ??
    (target.paneId
      ? host.agents.find(
          (a) => a.pane === target.paneId || a.remote_id === target.paneId
        )
      : undefined) ??
    host.agents[0];
  if (match) {
    runtime.selectAgent(target.hostId, match.remote_id);
  } else {
    runtime.selectHost(target.hostId);
  }
  useMobilePanelsStore.getState().selectPanel("agent");
  return true;
}

export function flushPendingNotificationDeepLink(): void {
  if (!pendingDeepLink) return;
  if (applyDeepLinkTarget(pendingDeepLink)) {
    pendingDeepLink = null;
  }
}

export function subscribeNotificationDeepLinks(): () => void {
  if (Platform.OS === "web") return () => {};
  ensureNotificationHandler();

  const handleResponse = (response: Notifications.NotificationResponse | null) => {
    if (!response) return;
    const target = extractDeepLinkTarget(
      response.notification.request.content.data
    );
    if (!target) return;
    if (!applyDeepLinkTarget(target)) {
      pendingDeepLink = target;
    }
  };

  void Notifications.getLastNotificationResponseAsync()
    .then(handleResponse)
    .catch(() => {});

  const sub = Notifications.addNotificationResponseReceivedListener(
    handleResponse
  );
  return () => sub.remove();
}

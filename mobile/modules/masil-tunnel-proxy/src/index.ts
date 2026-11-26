import { requireOptionalNativeModule } from "expo-modules-core";

export interface TunnelListenerHandle {
  listenerId: string;
  localPort: number;
  targetPort: number;
}

export interface TunnelStreamOpenEvent {
  listenerId: string;
  streamId: number;
  targetPort: number;
}

export interface TunnelStreamDataEvent {
  streamId: number;
  dataBase64: string;
}

export interface TunnelStreamCloseEvent {
  streamId: number;
  reason: string;
}

export interface NativeTunnelProxyModule {
  startListener(targetPort: number): Promise<TunnelListenerHandle>;
  rebindListener(listenerId: string): Promise<TunnelListenerHandle>;
  stopListener(listenerId: string): Promise<void>;
  writeStream(streamId: number, dataBase64: string): void;
  closeStream(streamId: number): void;
  addListener?(
    eventName: string,
    listener: (payload: Record<string, unknown>) => void
  ): { remove: () => void };
}

const nativeMod =
  requireOptionalNativeModule<NativeTunnelProxyModule>("MasilTunnelProxy");

export function isNativeTunnelProxyAvailable(): boolean {
  return nativeMod !== null;
}

export function getNativeTunnelProxy(): NativeTunnelProxyModule | null {
  return nativeMod;
}

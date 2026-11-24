/**
 * Records audio on the phone via `expo-audio` and streams it over E2EE binary
 * frames (`AudioChunk` / `AudioEnd`) to the host daemon's configured
 * `whisper.cpp` / STT command.
 */
import {
  AudioModule,
  RecordingPresets,
  setAudioModeAsync,
} from "expo-audio";
import { BinaryOpcode } from "../protocol/binary-frames";
import { MasilRemoteClient } from "../protocol/client";

const CHUNK_SIZE = 32 * 1024;
let nextAudioChannel = 500;

export async function ensureMicrophonePermission(): Promise<boolean> {
  const perm = await AudioModule.requestRecordingPermissionsAsync();
  if (!perm.granted) return false;
  await setAudioModeAsync({
    allowsRecording: true,
    playsInSilentMode: true,
  });
  return true;
}

export async function uploadRecordedAudioToHost(
  client: MasilRemoteClient,
  fileUri: string,
  targetAgent?: string,
  mode: "composer" | "queue" = "composer"
): Promise<number> {
  const response = await fetch(fileUri);
  const buffer = await response.arrayBuffer();
  const bytes = new Uint8Array(buffer);
  const channelId = nextAudioChannel++;
  if (nextAudioChannel > 60000) nextAudioChannel = 500;

  for (let off = 0; off < bytes.length; off += CHUNK_SIZE) {
    client.sendBinaryFrame({
      opcode: BinaryOpcode.AudioChunk,
      channelId,
      payload: bytes.subarray(off, Math.min(off + CHUNK_SIZE, bytes.length)),
    });
  }

  const endMeta = new TextEncoder().encode(
    JSON.stringify({
      mime: "audio/m4a",
      target_agent: targetAgent ?? null,
      mode,
    })
  );
  client.sendBinaryFrame({
    opcode: BinaryOpcode.AudioEnd,
    channelId,
    payload: endMeta,
  });

  return channelId;
}

export { RecordingPresets };

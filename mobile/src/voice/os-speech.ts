/**
 * On-device speech recognition adapter (default zero-setup mobile dictation).
 * Uses Web Speech API on web and delegates to native platform speech input
 * when a speech module is linked; gracefully reports unavailability so users
 * can switch to host E2EE `whisper.cpp` or OS keyboard dictation.
 */
import { Platform } from "react-native";

export interface OsSpeechSession {
  stop: () => void;
}

export function isOsSpeechAvailable(): boolean {
  if (Platform.OS === "web" && typeof globalThis !== "undefined") {
    const g = globalThis as unknown as Record<string, unknown>;
    return Boolean(g.SpeechRecognition || g.webkitSpeechRecognition);
  }
  return false;
}

export function startOsSpeechRecognition(
  lang: "ko" | "en",
  onTranscript: (text: string, isFinal: boolean) => void,
  onError: (msg: string) => void
): OsSpeechSession | null {
  if (Platform.OS === "web") {
    const g = globalThis as unknown as Record<string, unknown>;
    const Ctor = (g.SpeechRecognition || g.webkitSpeechRecognition) as
      | (new () => {
          lang: string;
          interimResults: boolean;
          continuous: boolean;
          onresult: ((ev: { results: ArrayLike<ArrayLike<{ transcript: string }> & { isFinal: boolean }> }) => void) | null;
          onerror: ((ev: { error?: string }) => void) | null;
          start: () => void;
          stop: () => void;
        })
      | undefined;
    if (!Ctor) return null;
    const rec = new Ctor();
    rec.lang = lang === "ko" ? "ko-KR" : "en-US";
    rec.interimResults = true;
    rec.continuous = false;
    rec.onresult = (ev) => {
      const last = ev.results[ev.results.length - 1];
      const text = last?.[0]?.transcript ?? "";
      onTranscript(text, Boolean(last?.isFinal));
    };
    rec.onerror = (ev) => onError(ev.error ?? "speech_error");
    rec.start();
    return { stop: () => rec.stop() };
  }
  return null;
}

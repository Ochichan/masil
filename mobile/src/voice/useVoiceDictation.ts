import { useCallback, useEffect, useRef, useState } from "react";
import { useAudioRecorder } from "expo-audio";
import {
  getHostClient,
  subscribeVoiceTranscriptions,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import {
  RecordingPresets,
  ensureMicrophonePermission,
  uploadRecordedAudioToHost,
} from "./host-stt-upload";
import {
  OsSpeechSession,
  isOsSpeechAvailable,
  startOsSpeechRecognition,
} from "./os-speech";

export function useVoiceDictation(
  hostId: string | null,
  targetAgent: string | undefined,
  lang: "ko" | "en",
  onText: (text: string) => void
) {
  const voiceMode = useHostRuntimeStore((s) => s.voiceMode);
  const recorder = useAudioRecorder(RecordingPresets.HIGH_QUALITY);
  const [recording, setRecording] = useState(false);
  const [transcribing, setTranscribing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const osSessionRef = useRef<OsSpeechSession | null>(null);
  const pendingChannelRef = useRef<number | null>(null);

  useEffect(() => {
    return subscribeVoiceTranscriptions((ev) => {
      if (pendingChannelRef.current !== ev.channelId) return;
      pendingChannelRef.current = null;
      setTranscribing(false);
      if (ev.ok && ev.text) {
        onText(ev.text);
      } else if (ev.error) {
        setError(ev.error);
      }
    });
  }, [onText]);

  const start = useCallback(async () => {
    setError(null);
    if (voiceMode === "os" && isOsSpeechAvailable()) {
      const sess = startOsSpeechRecognition(
        lang,
        (text, isFinal) => {
          if (isFinal) {
            setRecording(false);
            if (text.trim()) onText(text.trim());
          }
        },
        (msg) => {
          setRecording(false);
          setError(msg);
        }
      );
      if (sess) {
        osSessionRef.current = sess;
        setRecording(true);
        return;
      }
    }

    // Host E2EE STT recording path (also fallback when OS Speech API is not exposed).
    const ok = await ensureMicrophonePermission();
    if (!ok) {
      setError("Microphone permission denied");
      return;
    }
    await recorder.prepareToRecordAsync();
    recorder.record();
    setRecording(true);
  }, [lang, onText, recorder, voiceMode]);

  const stop = useCallback(async () => {
    if (osSessionRef.current) {
      osSessionRef.current.stop();
      osSessionRef.current = null;
      setRecording(false);
      return;
    }
    if (!recording) return;
    setRecording(false);
    try {
      await recorder.stop();
      const uri = recorder.uri;
      if (!uri || !hostId) return;
      const client = getHostClient(hostId);
      if (!client) {
        setError("Host is disconnected");
        return;
      }
      setTranscribing(true);
      pendingChannelRef.current = await uploadRecordedAudioToHost(
        client,
        uri,
        targetAgent,
        "composer"
      );
    } catch (e) {
      setTranscribing(false);
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [hostId, recorder, recording, targetAgent]);

  return { recording, transcribing, error, start, stop };
}

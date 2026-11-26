import React, { useCallback, useEffect, useState } from "react";
import {
  ActivityIndicator,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { CameraView, useCameraPermissions } from "expo-camera";
import * as Clipboard from "expo-clipboard";
import { useRouter } from "expo-router";
import { useTranslation } from "react-i18next";
import { SafeAreaView } from "react-native-safe-area-context";
import { useHostRuntimeStore } from "../hosts/HostRuntimeController";
import { registerPushTokenWithHost } from "../notifications/push-manager";
import { useMasilTheme } from "../theme/ThemeProvider";

export default function PairScreen() {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const router = useRouter();
  const pairFromUri = useHostRuntimeStore((s) => s.pairFromUri);
  const [permission, requestPermission] = useCameraPermissions();
  const [uriInput, setUriInput] = useState("");
  const [scanning, setScanning] = useState(true);
  const [busy, setBusy] = useState(false);
  const [statusNote, setStatusNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const completePairing = useCallback(
    async (uri: string) => {
      if (busy) return;
      const trimmed = uri.trim();
      setBusy(true);
      setError(null);
      setStatusNote(t("qrRecognizedPairing"));
      setScanning(false);
      setUriInput(trimmed);
      try {
        const saved = await pairFromUri(trimmed);
        void registerPushTokenWithHost(saved.hostId);
        router.back();
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
        setStatusNote(null);
        setScanning(true);
      } finally {
        setBusy(false);
      }
    },
    [busy, pairFromUri, router, t]
  );

  const handleScannedData = useCallback(
    (raw: string) => {
      const data = raw.trim();
      if (data.startsWith("masil://pair?")) {
        void completePairing(data);
      } else if (data.length > 0) {
        setStatusNote(t("qrNotMasil", { preview: data.slice(0, 28) }));
      }
    },
    [completePairing, t]
  );

  useEffect(() => {
    if (!CameraView.isModernBarcodeScannerAvailable) return;
    const sub = CameraView.onModernBarcodeScanned((result) => {
      if (result?.data) {
        handleScannedData(result.data);
      }
    });
    return () => {
      sub.remove();
    };
  }, [handleScannedData]);

  const openSystemScanner = async () => {
    setError(null);
    try {
      await CameraView.launchScanner({ barcodeTypes: ["qr"] });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <SafeAreaView style={[styles.root, { backgroundColor: palette.canvas }]}>
      <View
        style={[
          styles.header,
          { backgroundColor: palette.panel, borderBottomColor: palette.track },
        ]}
      >
        <Text style={[styles.title, { color: palette.text }]}>
          {t("pairHost")}
        </Text>
        <Pressable onPress={() => router.back()}>
          <Text style={{ color: palette.accent, fontWeight: "700" }}>
            {t("cancel")}
          </Text>
        </Pressable>
      </View>

      <View style={styles.cameraBox}>
        {permission?.granted ? (
          <>
            <CameraView
              style={StyleSheet.absoluteFill}
              facing="back"
              barcodeScannerSettings={{ barcodeTypes: ["qr"] }}
              onMountError={(ev) => setError(ev.message)}
              onBarcodeScanned={
                scanning && !busy
                  ? ({ data }) => handleScannedData(data)
                  : undefined
              }
            />
            <View pointerEvents="none" style={styles.reticleOverlay}>
              <View
                style={[styles.reticle, { borderColor: palette.accent }]}
              />
              <View style={styles.statusPill}>
                <Text style={styles.statusPillText}>
                  {statusNote ?? t("qrAlignInBox")}
                </Text>
              </View>
            </View>
          </>
        ) : (
          <View style={styles.permBox}>
            <Text style={{ color: palette.muted, textAlign: "center" }}>
              {t("cameraPermissionNeeded")}
            </Text>
            <Pressable
              onPress={() => void requestPermission()}
              style={[styles.btn, { backgroundColor: palette.accent }]}
            >
              <Text style={{ color: palette.accentText, fontWeight: "700" }}>
                {t("scanQr")}
              </Text>
            </Pressable>
          </View>
        )}
      </View>

      <View style={styles.pasteSection}>
        {CameraView.isModernBarcodeScannerAvailable ? (
          <Pressable
            onPress={() => void openSystemScanner()}
            disabled={busy}
            style={[
              styles.btn,
              {
                backgroundColor: palette.raised,
                borderWidth: 1,
                borderColor: palette.accent,
              },
            ]}
          >
            <Text style={{ color: palette.accent, fontWeight: "700" }}>
              {t("openSystemQrScanner")}
            </Text>
          </Pressable>
        ) : null}

        <Text style={[styles.label, { color: palette.muted }]}>
          {t("pasteUri")}
        </Text>
        <TextInput
          value={uriInput}
          onChangeText={setUriInput}
          placeholder="masil://pair?v=1&..."
          placeholderTextColor={palette.muted}
          autoCapitalize="none"
          autoCorrect={false}
          style={[
            styles.input,
            {
              backgroundColor: palette.panel,
              borderColor: palette.track,
              color: palette.text,
            },
          ]}
        />
        <View style={styles.pasteActions}>
          <Pressable
            onPress={async () => {
              const clip = await Clipboard.getStringAsync();
              if (clip) setUriInput(clip.trim());
            }}
            style={[
              styles.btn,
              {
                backgroundColor: palette.raised,
                borderWidth: 1,
                borderColor: palette.track,
                flex: 1,
              },
            ]}
          >
            <Text style={{ color: palette.text, fontWeight: "600" }}>
              {t("paste")}
            </Text>
          </Pressable>
          <Pressable
            onPress={() => void completePairing(uriInput)}
            disabled={!uriInput.trim() || busy}
            style={[
              styles.btn,
              {
                backgroundColor: palette.accent,
                flex: 1,
                opacity: uriInput.trim() && !busy ? 1 : 0.5,
              },
            ]}
          >
            {busy ? (
              <ActivityIndicator size="small" color={palette.accentText} />
            ) : (
              <Text style={{ color: palette.accentText, fontWeight: "700" }}>
                {t("connect")}
              </Text>
            )}
          </Pressable>
        </View>
        {error ? (
          <Text style={{ color: palette.danger, fontSize: 12 }}>{error}</Text>
        ) : null}
      </View>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  header: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    paddingHorizontal: 16,
    paddingVertical: 12,
    borderBottomWidth: 1,
  },
  title: { fontSize: 16, fontWeight: "700" },
  cameraBox: {
    flex: 1,
    margin: 16,
    borderRadius: 14,
    overflow: "hidden",
    backgroundColor: "#000",
  },
  reticleOverlay: {
    position: "absolute",
    top: 0,
    right: 0,
    bottom: 0,
    left: 0,
    alignItems: "center",
    justifyContent: "center",
    gap: 16,
  },
  reticle: {
    width: 220,
    height: 220,
    borderRadius: 16,
    borderWidth: 2,
    backgroundColor: "transparent",
  },
  statusPill: {
    backgroundColor: "rgba(12, 16, 22, 0.78)",
    paddingHorizontal: 14,
    paddingVertical: 6,
    borderRadius: 999,
  },
  statusPillText: {
    color: "#e6ebf1",
    fontSize: 12,
    fontWeight: "600",
  },
  permBox: {
    flex: 1,
    alignItems: "center",
    justifyContent: "center",
    padding: 20,
    gap: 12,
  },
  pasteSection: { padding: 16, gap: 10 },
  label: { fontSize: 12, fontWeight: "600" },
  input: {
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 10,
    paddingVertical: 9,
    fontSize: 13,
    fontFamily: "monospace",
  },
  pasteActions: { flexDirection: "row", gap: 10 },
  btn: {
    paddingVertical: 11,
    paddingHorizontal: 16,
    borderRadius: 8,
    alignItems: "center",
    justifyContent: "center",
  },
});

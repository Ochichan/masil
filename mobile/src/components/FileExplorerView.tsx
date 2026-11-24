import React, { useEffect, useState } from "react";
import {
  ActivityIndicator,
  FlatList,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { ChevronLeft, FileCode, Folder, Paperclip } from "lucide-react-native";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import type { FileEntry } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";

interface FileExplorerViewProps {
  hostId: string;
  cwd: string;
}

export function FileExplorerView({ hostId, cwd }: FileExplorerViewProps) {
  const { palette } = useMasilTheme();
  const [relPath, setRelPath] = useState("");
  const [entries, setEntries] = useState<FileEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [preview, setPreview] = useState<{
    path: string;
    content: string;
    binary: boolean;
    truncated: boolean;
  } | null>(null);
  const setAttachment = useHostRuntimeStore((s) => s.setComposerAttachment);
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);

  useEffect(() => {
    const client = getHostClient(hostId);
    if (!client || client.connectionState !== "connected" || !cwd) return;
    setLoading(true);
    void client
      .request<{ entries?: FileEntry[] }>("workspace.files.list.request", {
        cwd,
        path: relPath,
      })
      .then((res) => setEntries(res.entries ?? []))
      .catch(() => setEntries([]))
      .finally(() => setLoading(false));
  }, [hostId, cwd, relPath]);

  const openFile = async (entry: FileEntry) => {
    if (entry.is_dir) {
      setRelPath(entry.path);
      return;
    }
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      const res = await client.request<{
        path: string;
        content: string;
        binary: boolean;
        truncated: boolean;
      }>("workspace.files.read.request", { cwd, path: entry.path });
      setPreview(res);
    } catch {
      // ignore
    }
  };

  const goUp = () => {
    if (!relPath) return;
    const parts = relPath.split("/").filter(Boolean);
    parts.pop();
    setRelPath(parts.join("/"));
  };

  return (
    <View style={styles.root}>
      <View
        style={[
          styles.breadcrumb,
          { backgroundColor: palette.panel, borderBottomColor: palette.track },
        ]}
      >
        {relPath ? (
          <Pressable onPress={goUp} style={styles.upBtn}>
            <ChevronLeft size={16} color={palette.accent} />
            <Text style={{ color: palette.accent, fontWeight: "600" }}>Up</Text>
          </Pressable>
        ) : null}
        <Text
          numberOfLines={1}
          style={[styles.pathText, { color: palette.muted }]}
        >
          /{relPath}
        </Text>
      </View>

      {loading ? (
        <ActivityIndicator
          style={{ marginTop: 24 }}
          color={palette.accent}
        />
      ) : (
        <FlatList
          data={entries}
          keyExtractor={(item) => item.path}
          renderItem={({ item }) => (
            <Pressable
              onPress={() => void openFile(item)}
              style={[
                styles.row,
                { borderBottomColor: palette.track },
              ]}
            >
              {item.is_dir ? (
                <Folder size={16} color={palette.accent} />
              ) : (
                <FileCode size={16} color={palette.muted} />
              )}
              <Text
                numberOfLines={1}
                style={[styles.name, { color: palette.text }]}
              >
                {item.name}
              </Text>
              {!item.is_dir ? (
                <Pressable
                  onPress={() => {
                    setAttachment(item.path);
                    selectPanel("agent");
                  }}
                  hitSlop={8}
                  style={styles.attachBtn}
                >
                  <Paperclip size={14} color={palette.accent} />
                </Pressable>
              ) : null}
            </Pressable>
          )}
        />
      )}

      <Modal
        visible={preview !== null}
        animationType="slide"
        onRequestClose={() => setPreview(null)}
      >
        <View style={[styles.modalRoot, { backgroundColor: palette.canvas }]}>
          <View
            style={[
              styles.modalHeader,
              {
                backgroundColor: palette.panel,
                borderBottomColor: palette.track,
              },
            ]}
          >
            <Text
              numberOfLines={1}
              style={[styles.modalTitle, { color: palette.text }]}
            >
              {preview?.path}
            </Text>
            <View style={{ flexDirection: "row", gap: 14 }}>
              <Pressable
                onPress={() => {
                  if (preview) {
                    setAttachment(preview.path);
                    setPreview(null);
                    selectPanel("agent");
                  }
                }}
              >
                <Text style={{ color: palette.accent, fontWeight: "700" }}>
                  Attach
                </Text>
              </Pressable>
              <Pressable onPress={() => setPreview(null)}>
                <Text style={{ color: palette.muted, fontWeight: "700" }}>
                  Close
                </Text>
              </Pressable>
            </View>
          </View>
          <ScrollView style={styles.modalBody}>
            <Text
              selectable
              style={[styles.codeText, { color: palette.text }]}
            >
              {preview?.binary
                ? "(Binary file preview unavailable)"
                : preview?.content}
            </Text>
          </ScrollView>
        </View>
      </Modal>
    </View>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  breadcrumb: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 10,
    paddingVertical: 8,
    borderBottomWidth: 1,
    gap: 8,
  },
  upBtn: { flexDirection: "row", alignItems: "center", gap: 2 },
  pathText: { flex: 1, fontSize: 12, fontFamily: "monospace" },
  row: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 12,
    paddingVertical: 11,
    borderBottomWidth: StyleSheet.hairlineWidth,
    gap: 10,
  },
  name: { flex: 1, fontSize: 14 },
  attachBtn: { padding: 4 },
  modalRoot: { flex: 1, paddingTop: 44 },
  modalHeader: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    paddingHorizontal: 14,
    paddingVertical: 12,
    borderBottomWidth: 1,
    gap: 8,
  },
  modalTitle: { flex: 1, fontSize: 13, fontFamily: "monospace" },
  modalBody: { flex: 1, padding: 12 },
  codeText: { fontFamily: "monospace", fontSize: 12, lineHeight: 18 },
});

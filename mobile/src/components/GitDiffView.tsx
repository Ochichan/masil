import React, { useEffect, useState } from "react";
import {
  ActivityIndicator,
  FlatList,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  View,
} from "react-native";
import {
  CheckCircle2,
  ChevronLeft,
  FolderOpen,
  RefreshCw,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { getHostClient } from "../hosts/HostRuntimeController";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS, TABULAR_NUMS } from "../theme/typography";

interface ChangedFile {
  path: string;
  status?: string;
  added?: number;
  removed?: number;
}

interface GitDiffViewProps {
  hostId: string;
  cwd: string;
  onOpenFiles?: () => void;
}

export function GitDiffView({ hostId, cwd, onOpenFiles }: GitDiffViewProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const [files, setFiles] = useState<ChangedFile[]>([]);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [diffText, setDiffText] = useState<string>("");
  const [loading, setLoading] = useState(false);

  const loadChanges = async () => {
    const client = getHostClient(hostId);
    if (!client || client.connectionState !== "connected" || !cwd) return;
    setLoading(true);
    try {
      const res = await client.request<{ files?: ChangedFile[] }>(
        "workspace.changes.list.request",
        { cwd }
      );
      setFiles(res.files ?? []);
    } catch {
      setFiles([]);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void loadChanges();
  }, [hostId, cwd]);

  const openDiff = async (path: string) => {
    setSelectedPath(path);
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      const res = await client.request<{ diff?: string; text?: string }>(
        "workspace.changes.diff.request",
        { cwd, path }
      );
      setDiffText(res.diff ?? res.text ?? t("diffEmptyContent"));
    } catch (e) {
      setDiffText(e instanceof Error ? e.message : String(e));
    }
  };

  if (selectedPath) {
    const lines = diffText.split("\n");
    return (
      <View style={styles.root}>
        <View
          style={[
            styles.diffHeader,
            { backgroundColor: palette.panel, borderBottomColor: palette.hairline },
          ]}
        >
          <Pressable
            onPress={() => setSelectedPath(null)}
            style={styles.backRow}
            hitSlop={8}
          >
            <ChevronLeft size={16} color={palette.accent} />
            <Text style={[styles.backText, { color: palette.accent }]}>
              {t("backToList")}
            </Text>
          </Pressable>
          <Text
            numberOfLines={1}
            ellipsizeMode="middle"
            style={[styles.path, { color: palette.text }]}
          >
            {selectedPath}
          </Text>
        </View>
        <ScrollView style={styles.diffScroll}>
          {lines.map((line, idx) => {
            const isAdd = line.startsWith("+") && !line.startsWith("+++");
            const isDel = line.startsWith("-") && !line.startsWith("---");
            const isHunk = line.startsWith("@@");
            const color = isAdd
              ? palette.success
              : isDel
                ? palette.danger
                : isHunk
                  ? palette.accent
                  : palette.text;
            const bg = isAdd
              ? palette.successSoft
              : isDel
                ? palette.dangerSoft
                : "transparent";
            return (
              <Text
                key={idx}
                selectable
                style={[styles.diffLine, { color, backgroundColor: bg }]}
              >
                {line}
              </Text>
            );
          })}
        </ScrollView>
      </View>
    );
  }

  if (loading && files.length === 0) {
    return (
      <View style={styles.emptyRoot}>
        <ActivityIndicator size="small" color={palette.accent} />
        <Text style={[styles.emptySub, { color: palette.muted }]}>
          {t("loadingGitChanges")}
        </Text>
      </View>
    );
  }

  return (
    <View style={styles.root}>
      <FlatList
        data={files}
        keyExtractor={(item) => item.path}
        contentContainerStyle={
          files.length === 0 ? styles.emptyContainer : styles.listContainer
        }
        ListEmptyComponent={
          <View style={styles.emptyRoot}>
            <View
              style={[
                styles.emptyIconCircle,
                {
                  backgroundColor: palette.successSoft,
                  borderColor: palette.hairline,
                },
              ]}
            >
              <CheckCircle2 size={24} color={palette.success} />
            </View>
            <Text style={[styles.emptyTitle, { color: palette.text }]}>
              {t("noChangedFilesTitle")}
            </Text>
            <Text style={[styles.emptySub, { color: palette.muted }]}>
              {t("noChangedFilesBody")}
            </Text>
            <View style={styles.emptyActions}>
              <Pressable
                onPress={() => void loadChanges()}
                style={[
                  styles.emptyBtn,
                  {
                    backgroundColor: "transparent",
                    borderColor: palette.hairline,
                  },
                ]}
              >
                <RefreshCw size={14} color={palette.muted} />
                <Text style={[styles.emptyBtnText, { color: palette.muted }]}>
                  {t("refresh")}
                </Text>
              </Pressable>
              {onOpenFiles ? (
                <Pressable
                  onPress={onOpenFiles}
                  style={[
                    styles.emptyBtn,
                    {
                      backgroundColor: palette.raised,
                      borderColor: palette.borderSubtle,
                    },
                  ]}
                >
                  <FolderOpen size={14} color={palette.accent} />
                  <Text style={[styles.emptyBtnText, { color: palette.text }]}>
                    {t("openFileExplorer")}
                  </Text>
                </Pressable>
              ) : null}
            </View>
          </View>
        }
        renderItem={({ item }) => (
          <Pressable
            onPress={() => void openDiff(item.path)}
            style={[
              styles.fileRow,
              {
                backgroundColor: palette.panel,
                borderColor: palette.hairline,
              },
            ]}
          >
            <Text style={[styles.statusTag, { color: palette.warning }]}>
              {item.status ?? "M"}
            </Text>
            <Text
              numberOfLines={1}
              ellipsizeMode="middle"
              style={[styles.filePath, { color: palette.text }]}
            >
              {item.path}
            </Text>
            {typeof item.added === "number" ? (
              <Text style={[styles.statNum, { color: palette.success }]}>
                +{item.added}
              </Text>
            ) : null}
            {typeof item.removed === "number" ? (
              <Text style={[styles.statNum, { color: palette.danger }]}>
                -{item.removed}
              </Text>
            ) : null}
          </Pressable>
        )}
      />
    </View>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  diffHeader: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 12,
    paddingVertical: 10,
    borderBottomWidth: 1,
    gap: 10,
  },
  backRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 2,
  },
  backText: {
    fontFamily: FONTS.semi,
    fontSize: 13,
  },
  path: {
    flex: 1,
    fontSize: 12,
    fontFamily: FONTS.mono,
  },
  diffScroll: { flex: 1, padding: 10 },
  diffLine: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
    lineHeight: 17.5,
    paddingHorizontal: 5,
  },
  listContainer: {
    padding: 14,
    gap: 8,
  },
  emptyContainer: {
    flexGrow: 1,
    justifyContent: "flex-start",
    paddingTop: 72,
  },
  emptyRoot: {
    alignItems: "center",
    paddingHorizontal: 28,
    paddingVertical: 16,
    gap: 8,
  },
  emptyIconCircle: {
    width: 48,
    height: 48,
    borderRadius: 24,
    borderWidth: 1,
    alignItems: "center",
    justifyContent: "center",
    marginBottom: 4,
  },
  emptyTitle: {
    fontFamily: FONTS.bold,
    fontSize: 16,
    letterSpacing: -0.2,
    textAlign: "center",
  },
  emptySub: {
    fontFamily: FONTS.regular,
    fontSize: 13.5,
    lineHeight: 20,
    textAlign: "center",
    maxWidth: 300,
  },
  cwdPill: {
    marginTop: 4,
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 10,
    paddingVertical: 5,
    maxWidth: "90%",
  },
  cwdPillText: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
  },
  emptyActions: {
    flexDirection: "row",
    alignItems: "center",
    gap: 10,
    marginTop: 10,
  },
  emptyBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 14,
    height: 40,
  },
  emptyBtnText: {
    fontFamily: FONTS.semi,
    fontSize: 13,
  },
  fileRow: {
    flexDirection: "row",
    alignItems: "center",
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 12,
    paddingVertical: 11,
    gap: 8,
  },
  statusTag: {
    fontFamily: FONTS.monoSemi,
    fontSize: 12,
    width: 20,
  },
  filePath: {
    flex: 1,
    fontSize: 12.5,
    fontFamily: FONTS.mono,
  },
  statNum: {
    fontFamily: FONTS.monoSemi,
    fontVariant: TABULAR_NUMS,
    fontSize: 12,
  },
});

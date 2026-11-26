import React, { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  ActivityIndicator,
  FlatList,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import {
  ArrowUp,
  Check,
  ChevronRight,
  Eye,
  EyeOff,
  Folder,
  FolderGit2,
  FolderPlus,
  Home,
  Search,
  X,
} from "lucide-react-native";
import { getHostClient } from "../hosts/HostRuntimeController";
import { matchesMasilQuery } from "../search/korean-choseong";
import { useMasilTheme } from "../theme/ThemeProvider";

export interface DirEntryItem {
  name: string;
  path: string;
  display_path: string;
  is_git_repo: boolean;
  source?: "active" | "recent" | "bookmark";
}

interface BreadcrumbItem {
  label: string;
  path: string;
}

interface BrowseDirsResponse {
  home: string;
  current: string;
  display_current: string;
  current_is_git_repo: boolean;
  parent: string | null;
  breadcrumbs: BreadcrumbItem[];
  entries: DirEntryItem[];
  quick: DirEntryItem[];
  truncated: boolean;
}

interface DirectoryPickerModalProps {
  visible: boolean;
  hostId: string;
  initialPath?: string;
  onSelect: (dir: DirEntryItem) => void;
  onClose: () => void;
}

export function DirectoryPickerModal({
  visible,
  hostId,
  initialPath,
  onSelect,
  onClose,
}: DirectoryPickerModalProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const [currentPath, setCurrentPath] = useState(initialPath ?? "");
  const [data, setData] = useState<BrowseDirsResponse | null>(null);
  const [quickDirs, setQuickDirs] = useState<DirEntryItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [showHidden, setShowHidden] = useState(false);
  const [creatingDir, setCreatingDir] = useState(false);
  const [newDirName, setNewDirName] = useState("");
  const [mkdirBusy, setMkdirBusy] = useState(false);

  const loadDir = useCallback(
    async (targetPath: string, includeQuick: boolean, hidden: boolean) => {
      const client = getHostClient(hostId);
      if (!client) {
        setError(t("hostNotConnected"));
        return;
      }
      setLoading(true);
      setError(null);
      try {
        const res = await client.request<BrowseDirsResponse>(
          "host.dirs.list.request",
          {
            path: targetPath,
            show_hidden: hidden,
            include_quick: includeQuick,
          }
        );
        setData(res);
        setCurrentPath(res.current);
        if (includeQuick && res.quick.length > 0) {
          setQuickDirs(res.quick);
        }
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    },
    [hostId, t]
  );

  useEffect(() => {
    if (!visible) return;
    setQuery("");
    setCreatingDir(false);
    setNewDirName("");
    void loadDir(initialPath ?? "", true, showHidden);
  }, [visible, initialPath, showHidden, loadDir]);

  const filteredEntries = useMemo(() => {
    const list = data?.entries ?? [];
    if (!query.trim()) return list;
    return list.filter((e) =>
      matchesMasilQuery(`${e.name} ${e.display_path}`, query)
    );
  }, [data?.entries, query]);

  const handleCreateDir = async () => {
    const trimmed = newDirName.trim();
    if (!trimmed || !data) return;
    const client = getHostClient(hostId);
    if (!client) return;
    setMkdirBusy(true);
    setError(null);
    try {
      const created = await client.request<DirEntryItem>(
        "host.dirs.mkdir.request",
        {
          parent: data.current,
          name: trimmed,
        }
      );
      setCreatingDir(false);
      setNewDirName("");
      onSelect(created);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setMkdirBusy(false);
    }
  };

  const selectCurrent = () => {
    if (!data) return;
    const baseName =
      data.current.split("/").filter(Boolean).pop() || data.display_current;
    onSelect({
      name: baseName,
      path: data.current,
      display_path: data.display_current,
      is_git_repo: data.current_is_git_repo,
    });
  };

  return (
    <Modal
      visible={visible}
      transparent
      animationType="slide"
      onRequestClose={onClose}
    >
      <View style={styles.backdrop}>
        <View
          style={[
            styles.sheet,
            { backgroundColor: palette.canvas, borderColor: palette.track },
          ]}
        >
          {/* Header */}
          <View
            style={[
              styles.header,
              { backgroundColor: palette.panel, borderBottomColor: palette.track },
            ]}
          >
            <View style={{ flex: 1 }}>
              <Text style={[styles.title, { color: palette.text }]}>
                {t("selectWorkingFolder")}
              </Text>
              <Text
                numberOfLines={1}
                style={{ color: palette.muted, fontSize: 11, marginTop: 2 }}
              >
                {data?.display_current ?? (currentPath || t("loadingEllipsis"))}
              </Text>
            </View>
            <Pressable
              onPress={() => setCreatingDir((v) => !v)}
              style={[
                styles.iconBtn,
                {
                  backgroundColor: creatingDir ? palette.accent : palette.raised,
                  borderColor: palette.track,
                },
              ]}
            >
              <FolderPlus
                size={16}
                color={creatingDir ? palette.accentText : palette.text}
              />
            </Pressable>
            <Pressable
              onPress={() => setShowHidden((v) => !v)}
              style={[
                styles.iconBtn,
                {
                  backgroundColor: showHidden ? palette.accent : palette.raised,
                  borderColor: palette.track,
                },
              ]}
            >
              {showHidden ? (
                <Eye size={16} color={palette.accentText} />
              ) : (
                <EyeOff size={16} color={palette.muted} />
              )}
            </Pressable>
            <Pressable onPress={onClose} hitSlop={8} style={styles.closeBtn}>
              <X size={20} color={palette.muted} />
            </Pressable>
          </View>

          {/* Inline new folder bar */}
          {creatingDir ? (
            <View
              style={[
                styles.mkdirRow,
                { backgroundColor: palette.panel, borderBottomColor: palette.track },
              ]}
            >
              <TextInput
                value={newDirName}
                onChangeText={setNewDirName}
                placeholder={t("newFolderPlaceholder")}
                placeholderTextColor={palette.muted}
                autoFocus
                autoCapitalize="none"
                autoCorrect={false}
                onSubmitEditing={() => void handleCreateDir()}
                style={[
                  styles.mkdirInput,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.accent,
                    color: palette.text,
                  },
                ]}
              />
              <Pressable
                onPress={() => void handleCreateDir()}
                disabled={!newDirName.trim() || mkdirBusy}
                style={[
                  styles.mkdirSubmit,
                  {
                    backgroundColor: palette.accent,
                    opacity: newDirName.trim() && !mkdirBusy ? 1 : 0.5,
                  },
                ]}
              >
                <Text style={{ color: palette.accentText, fontWeight: "700", fontSize: 12 }}>
                  {mkdirBusy ? t("creating") : t("createAndSelect")}
                </Text>
              </Pressable>
            </View>
          ) : null}

          {/* Quick recent / bookmark chips */}
          {quickDirs.length > 0 ? (
            <View
              style={[
                styles.quickWrap,
                { backgroundColor: palette.panel, borderBottomColor: palette.track },
              ]}
            >
              <ScrollView
                horizontal
                showsHorizontalScrollIndicator={false}
                contentContainerStyle={styles.quickList}
              >
                {quickDirs.map((q) => {
                  const isCurrent = data?.current === q.path;
                  return (
                    <Pressable
                      key={q.path}
                      onPress={() => {
                        setQuery("");
                        void loadDir(q.path, false, showHidden);
                      }}
                      style={[
                        styles.quickChip,
                        {
                          backgroundColor: isCurrent
                            ? palette.accent
                            : palette.raised,
                          borderColor: isCurrent ? palette.accent : palette.track,
                        },
                      ]}
                    >
                      {q.is_git_repo ? (
                        <FolderGit2
                          size={13}
                          color={isCurrent ? palette.accentText : palette.accent}
                        />
                      ) : q.display_path === "~" ? (
                        <Home
                          size={13}
                          color={isCurrent ? palette.accentText : palette.muted}
                        />
                      ) : (
                        <Folder
                          size={13}
                          color={isCurrent ? palette.accentText : palette.muted}
                        />
                      )}
                      <Text
                        numberOfLines={1}
                        style={{
                          color: isCurrent ? palette.accentText : palette.text,
                          fontSize: 12,
                          fontWeight: "600",
                          maxWidth: 150,
                        }}
                      >
                        {q.name}
                      </Text>
                    </Pressable>
                  );
                })}
              </ScrollView>
            </View>
          ) : null}

          {/* Interactive breadcrumbs */}
          {data && data.breadcrumbs.length > 0 ? (
            <View
              style={[
                styles.crumbBar,
                { borderBottomColor: palette.track },
              ]}
            >
              {data.parent ? (
                <Pressable
                  onPress={() => {
                    setQuery("");
                    void loadDir(data.parent!, false, showHidden);
                  }}
                  style={[
                    styles.upBtn,
                    { backgroundColor: palette.raised, borderColor: palette.track },
                  ]}
                >
                  <ArrowUp size={14} color={palette.text} />
                </Pressable>
              ) : null}
              <ScrollView
                horizontal
                showsHorizontalScrollIndicator={false}
                contentContainerStyle={styles.crumbList}
              >
                {data.breadcrumbs.map((b, idx) => {
                  const last = idx === data.breadcrumbs.length - 1;
                  return (
                    <React.Fragment key={b.path}>
                      {idx > 0 ? (
                        <ChevronRight size={13} color={palette.muted} />
                      ) : null}
                      <Pressable
                        onPress={() => {
                          if (!last) {
                            setQuery("");
                            void loadDir(b.path, false, showHidden);
                          }
                        }}
                        style={[
                          styles.crumbChip,
                          {
                            backgroundColor: last
                              ? palette.raised
                              : "transparent",
                          },
                        ]}
                      >
                        <Text
                          style={{
                            color: last ? palette.accent : palette.muted,
                            fontSize: 12,
                            fontWeight: last ? "700" : "500",
                          }}
                        >
                          {b.label}
                        </Text>
                      </Pressable>
                    </React.Fragment>
                  );
                })}
              </ScrollView>
            </View>
          ) : null}

          {/* Instant filter input */}
          <View style={styles.searchRow}>
            <View
              style={[
                styles.searchBox,
                { backgroundColor: palette.panel, borderColor: palette.track },
              ]}
            >
              <Search size={14} color={palette.muted} />
              <TextInput
                value={query}
                onChangeText={setQuery}
                placeholder={t("searchFoldersPlaceholder")}
                placeholderTextColor={palette.muted}
                autoCapitalize="none"
                autoCorrect={false}
                style={[styles.searchInput, { color: palette.text }]}
              />
              {query.length > 0 ? (
                <Pressable onPress={() => setQuery("")} hitSlop={6}>
                  <X size={14} color={palette.muted} />
                </Pressable>
              ) : null}
            </View>
          </View>

          {error ? (
            <Text style={{ color: palette.danger, fontSize: 12, paddingHorizontal: 14, paddingBottom: 8 }}>
              {error}
            </Text>
          ) : null}

          {/* Folder list */}
          {loading && !data ? (
            <View style={styles.loadingBox}>
              <ActivityIndicator color={palette.accent} />
            </View>
          ) : (
            <FlatList
              data={filteredEntries}
              keyExtractor={(item) => item.path}
              contentContainerStyle={styles.listContent}
              ListEmptyComponent={
                <View style={styles.emptyBox}>
                  <Text style={{ color: palette.muted, fontSize: 13 }}>
                    {query.trim()
                      ? t("noMatchingSubfolders")
                      : t("noSubfoldersHint")}
                  </Text>
                </View>
              }
              renderItem={({ item }) => (
                <View
                  style={[
                    styles.row,
                    { backgroundColor: palette.panel, borderColor: palette.track },
                  ]}
                >
                  <Pressable
                    onPress={() => {
                      setQuery("");
                      void loadDir(item.path, false, showHidden);
                    }}
                    style={styles.rowMain}
                  >
                    {item.is_git_repo ? (
                      <FolderGit2 size={18} color={palette.accent} />
                    ) : (
                      <Folder size={18} color={palette.muted} />
                    )}
                    <View style={{ flex: 1 }}>
                      <Text
                        numberOfLines={1}
                        style={[styles.rowName, { color: palette.text }]}
                      >
                        {item.name}
                      </Text>
                      {item.is_git_repo ? (
                        <Text style={{ color: palette.accent, fontSize: 10, fontWeight: "700" }}>
                          GIT REPO
                        </Text>
                      ) : null}
                    </View>
                    <ChevronRight size={16} color={palette.muted} />
                  </Pressable>
                  <Pressable
                    onPress={() => onSelect(item)}
                    style={[
                      styles.pickRowBtn,
                      {
                        backgroundColor: item.is_git_repo
                          ? palette.accent
                          : palette.raised,
                        borderColor: item.is_git_repo
                          ? palette.accent
                          : palette.track,
                      },
                    ]}
                  >
                    <Check
                      size={13}
                      color={item.is_git_repo ? palette.accentText : palette.text}
                    />
                    <Text
                      style={{
                        color: item.is_git_repo ? palette.accentText : palette.text,
                        fontSize: 12,
                        fontWeight: "700",
                      }}
                    >
                      {t("select")}
                    </Text>
                  </Pressable>
                </View>
              )}
            />
          )}

          {/* Sticky bottom select-current-directory bar */}
          <View
            style={[
              styles.footer,
              { backgroundColor: palette.panel, borderTopColor: palette.track },
            ]}
          >
            <View style={{ flex: 1 }}>
              <View style={{ flexDirection: "row", alignItems: "center", gap: 6 }}>
                <Text style={{ color: palette.muted, fontSize: 11, fontWeight: "600" }}>
                  {t("currentLocation")}
                </Text>
                {data?.current_is_git_repo ? (
                  <Text style={{ color: palette.accent, fontSize: 10, fontWeight: "800" }}>
                    GIT
                  </Text>
                ) : null}
              </View>
              <Text
                numberOfLines={1}
                style={{ color: palette.text, fontSize: 13, fontWeight: "700" }}
              >
                {data?.display_current ?? "..."}
              </Text>
            </View>
            <Pressable
              onPress={selectCurrent}
              disabled={!data}
              style={[
                styles.selectCurrentBtn,
                { backgroundColor: palette.accent, opacity: data ? 1 : 0.5 },
              ]}
            >
              <Check size={15} color={palette.accentText} />
              <Text style={{ color: palette.accentText, fontWeight: "700", fontSize: 13 }}>
                {t("selectThisFolder")}
              </Text>
            </Pressable>
          </View>
        </View>
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: {
    flex: 1,
    backgroundColor: "rgba(0,0,0,0.62)",
    justifyContent: "flex-end",
  },
  sheet: {
    height: "88%",
    borderTopLeftRadius: 18,
    borderTopRightRadius: 18,
    borderWidth: 1,
    overflow: "hidden",
  },
  header: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 14,
    paddingVertical: 12,
    borderBottomWidth: 1,
    gap: 8,
  },
  title: { fontSize: 16, fontWeight: "700" },
  iconBtn: {
    width: 34,
    height: 34,
    borderRadius: 8,
    borderWidth: 1,
    alignItems: "center",
    justifyContent: "center",
  },
  closeBtn: {
    padding: 4,
  },
  mkdirRow: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 12,
    paddingVertical: 10,
    borderBottomWidth: 1,
    gap: 8,
  },
  mkdirInput: {
    flex: 1,
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 10,
    paddingVertical: 8,
    fontSize: 13,
  },
  mkdirSubmit: {
    paddingHorizontal: 12,
    paddingVertical: 9,
    borderRadius: 8,
  },
  quickWrap: {
    borderBottomWidth: 1,
    paddingVertical: 8,
  },
  quickList: {
    paddingHorizontal: 12,
    gap: 6,
  },
  quickChip: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    paddingHorizontal: 10,
    paddingVertical: 6,
    borderRadius: 999,
    borderWidth: 1,
  },
  crumbBar: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 12,
    paddingVertical: 7,
    borderBottomWidth: 1,
    gap: 6,
  },
  upBtn: {
    width: 28,
    height: 28,
    borderRadius: 6,
    borderWidth: 1,
    alignItems: "center",
    justifyContent: "center",
  },
  crumbList: {
    alignItems: "center",
    gap: 2,
  },
  crumbChip: {
    paddingHorizontal: 6,
    paddingVertical: 3,
    borderRadius: 6,
  },
  searchRow: {
    paddingHorizontal: 12,
    paddingVertical: 8,
  },
  searchBox: {
    flexDirection: "row",
    alignItems: "center",
    borderWidth: 1,
    borderRadius: 9,
    paddingHorizontal: 10,
    gap: 8,
  },
  searchInput: {
    flex: 1,
    paddingVertical: 8,
    fontSize: 13,
  },
  loadingBox: {
    flex: 1,
    alignItems: "center",
    justifyContent: "center",
  },
  listContent: {
    paddingHorizontal: 12,
    paddingBottom: 16,
    gap: 7,
  },
  emptyBox: {
    padding: 24,
    alignItems: "center",
  },
  row: {
    flexDirection: "row",
    alignItems: "center",
    borderWidth: 1,
    borderRadius: 10,
    paddingLeft: 12,
    paddingRight: 8,
    paddingVertical: 8,
    gap: 8,
  },
  rowMain: {
    flex: 1,
    flexDirection: "row",
    alignItems: "center",
    gap: 10,
    paddingVertical: 2,
  },
  rowName: {
    fontSize: 14,
    fontWeight: "600",
  },
  pickRowBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 4,
    paddingHorizontal: 10,
    paddingVertical: 7,
    borderRadius: 8,
    borderWidth: 1,
  },
  footer: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 14,
    paddingVertical: 12,
    borderTopWidth: 1,
    gap: 10,
  },
  selectCurrentBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    paddingHorizontal: 14,
    paddingVertical: 10,
    borderRadius: 10,
  },
});

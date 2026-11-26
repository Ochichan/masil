import React, { useEffect, useRef, useState } from "react";
import {
  ActivityIndicator,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import {
  ChevronDown,
  ChevronRight,
  Folder,
  FolderGit2,
  FolderOpen,
  GitBranch,
  Play,
  Shield,
  SlidersHorizontal,
  X,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import {
  DirEntryItem,
  DirectoryPickerModal,
} from "../components/DirectoryPickerModal";
import { ModelPickerDropdown } from "../components/ModelPickerDropdown";
import {
  CapsuleSwitch,
  SelectableChip,
  formatEffortLabel,
  formatModeLabel,
} from "../components/SelectableChip";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import type {
  PluginPresetDef,
  ProviderModelsResponse,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS, isScratchDirectory } from "../theme/typography";

const PRIMARY_PROVIDERS = [
  { id: "agy", label: "Antigravity (agy)" },
  { id: "claude", label: "Claude Code" },
  { id: "codex", label: "Codex" },
  { id: "opencode", label: "OpenCode" },
];

const EXTRA_PROVIDERS = [
  { id: "cursor", label: "Cursor" },
  { id: "gemini", label: "Gemini CLI" },
  { id: "omp", label: "OMP" },
  { id: "kilo", label: "Kilo" },
];

const ALL_PROVIDERS = [...PRIMARY_PROVIDERS, ...EXTRA_PROVIDERS];

interface NewAgentSheetProps {
  visible: boolean;
  hostId: string;
  onClose: () => void;
}

function suggestAgentName(dirName: string | undefined, provider: string): string {
  const cleanDir = (dirName ?? "")
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 18);
  return cleanDir ? `${cleanDir}-${provider}` : `${provider}-mobile`;
}

const EMPTY_PLUGIN_PRESETS: PluginPresetDef[] = [];

export function NewAgentSheet({
  visible,
  hostId,
  onClose,
}: NewAgentSheetProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const insets = useSafeAreaInsets();
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const selectAgent = useHostRuntimeStore((s) => s.selectAgent);
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);
  const presets = useHostRuntimeStore(
    (s) => s.hosts[hostId]?.plugins?.presets ?? EMPTY_PLUGIN_PRESETS
  );

  const [provider, setProvider] = useState("agy");
  const [activePresetId, setActivePresetId] = useState<string | null>(null);
  const [presetInitialPrompt, setPresetInitialPrompt] = useState<string | null>(
    null
  );
  const [selectedDir, setSelectedDir] = useState<DirEntryItem | null>(null);
  const [quickDirs, setQuickDirs] = useState<DirEntryItem[]>([]);
  const [loadingQuick, setLoadingQuick] = useState(false);
  const [showPicker, setShowPicker] = useState(false);
  const [name, setName] = useState("agy-mobile");
  const [nameEdited, setNameEdited] = useState(false);

  const [providerMeta, setProviderMeta] = useState<ProviderModelsResponse | null>(
    null
  );
  const [loadingModels, setLoadingModels] = useState(false);
  const [selectedModel, setSelectedModel] = useState("");
  const [selectedEffort, setSelectedEffort] = useState("");
  const [selectedMode, setSelectedMode] = useState("");
  const pendingPresetRef = useRef<PluginPresetDef | null>(null);
  const [showMoreProviders, setShowMoreProviders] = useState(false);
  const [showAdvanced, setShowAdvanced] = useState(false);

  const [createWorktree, setCreateWorktree] = useState(false);
  const [branch, setBranch] = useState("");
  const [sandboxEnabled, setSandboxEnabled] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!visible) return;
    setError(null);
    const client = getHostClient(hostId);
    if (!client) return;
    let cancelled = false;
    setLoadingQuick(true);
    client
      .request<{ quick?: DirEntryItem[] }>("host.dirs.list.request", {
        path: "",
        include_quick: true,
      })
      .then((res) => {
        if (cancelled) return;
        const seenNames = new Set<string>();
        const list = (res.quick ?? []).filter((d) => {
          if (isScratchDirectory(d.path, d.name)) return false;
          const key = d.name.trim().toLowerCase();
          if (seenNames.has(key)) return false;
          seenNames.add(key);
          return true;
        });
        setQuickDirs(list);
        if (!selectedDir && list.length > 0) {
          const firstRepo = list.find((d) => d.is_git_repo) ?? list[0]!;
          setSelectedDir(firstRepo);
          if (!nameEdited) {
            setName(suggestAgentName(firstRepo.name, provider));
          }
        }
      })
      .catch(() => {})
      .finally(() => {
        if (!cancelled) setLoadingQuick(false);
      });
    return () => {
      cancelled = true;
    };
  }, [visible, hostId]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!visible) return;
    const client = getHostClient(hostId);
    if (!client) return;
    let cancelled = false;
    setLoadingModels(true);
    const pendingPreset =
      pendingPresetRef.current && pendingPresetRef.current.provider === provider
        ? pendingPresetRef.current
        : null;
    pendingPresetRef.current = null;
    setSelectedModel(pendingPreset?.model ?? "");
    setSelectedEffort(pendingPreset?.effort ?? "");
    setSelectedMode(pendingPreset?.mode ?? "");
    client
      .request<ProviderModelsResponse>("host.models.list.request", {
        provider,
      })
      .then((res) => {
        if (!cancelled) setProviderMeta(res);
      })
      .catch(() => {
        if (!cancelled) setProviderMeta(null);
      })
      .finally(() => {
        if (!cancelled) setLoadingModels(false);
      });
    return () => {
      cancelled = true;
    };
  }, [visible, hostId, provider]);

  const handleChooseProvider = (nextProvider: string) => {
    pendingPresetRef.current = null;
    setActivePresetId(null);
    setPresetInitialPrompt(null);
    setProvider(nextProvider);
    if (!nameEdited) {
      setName(suggestAgentName(selectedDir?.name, nextProvider));
    }
  };

  const applyPreset = (preset: PluginPresetDef) => {
    setActivePresetId(`${preset.plugin_id}:${preset.id}`);
    setCreateWorktree(Boolean(preset.worktree));
    setPresetInitialPrompt(preset.initial_prompt ?? null);
    if (preset.effort || preset.mode) {
      setShowAdvanced(true);
    }
    if (!nameEdited) {
      setName(suggestAgentName(selectedDir?.name, preset.id));
    }
    if (preset.provider !== provider) {
      pendingPresetRef.current = preset;
      setProvider(preset.provider);
    } else {
      pendingPresetRef.current = null;
      setSelectedModel(preset.model ?? "");
      setSelectedEffort(preset.effort ?? "");
      setSelectedMode(preset.mode ?? "");
    }
  };

  const handleChooseDir = (dir: DirEntryItem) => {
    setSelectedDir(dir);
    setShowPicker(false);
    if (!dir.is_git_repo) {
      setCreateWorktree(false);
    }
    if (!nameEdited) {
      setName(suggestAgentName(dir.name, provider));
    }
  };

  const providerLabel =
    ALL_PROVIDERS.find((p) => p.id === provider)?.label ?? provider;

  const launch = async () => {
    const client = getHostClient(hostId);
    if (!client) {
      setError(t("hostNotConnected"));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      let targetCwd = selectedDir?.path.trim() ?? "";
      if (createWorktree && targetCwd && branch.trim()) {
        const wt = await client.request<{ path?: string }>(
          "worktree.create.request",
          { repo: targetCwd, branch: branch.trim() }
        );
        if (wt.path) targetCwd = wt.path;
      }
      const launchName =
        name.trim() || suggestAgentName(selectedDir?.name, provider);
      const started = await client.request<{
        pane?: string;
        pane_id?: string;
        name?: string;
      }>("agent.start.request", {
        name: launchName,
        provider,
        cwd: targetCwd || undefined,
        answers: provider === "opencode" || provider === "kilo",
        model: selectedModel || undefined,
        effort: selectedEffort || undefined,
        mode: selectedMode || undefined,
        sandbox: sandboxEnabled ? "worktree" : "off",
        no_sandbox: !sandboxEnabled,
        initial_prompt: presetInitialPrompt?.trim() || undefined,
      });
      await refreshHost(hostId, false);

      setTimeout(() => {
        void useHostRuntimeStore.getState().refreshHost(hostId, false);
      }, 1500);

      const latestHost = useHostRuntimeStore.getState().hosts[hostId];
      const paneHint = started.pane_id ?? started.pane;
      const found = latestHost?.agents.find(
        (a) =>
          (paneHint && a.pane === paneHint) ||
          a.name === (started.name ?? launchName)
      );
      if (found) {
        selectAgent(hostId, found.remote_id);
        selectPanel("agent");
      }
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
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
            { backgroundColor: palette.panel, borderColor: palette.borderSubtle },
          ]}
        >
          <View style={[styles.grabBar, { backgroundColor: palette.track }]} />

          <View style={styles.header}>
            <Text style={[styles.title, { color: palette.text }]}>
              {t("newAgent")}
            </Text>
            <Pressable onPress={onClose} hitSlop={8} style={styles.closeBtn}>
              <X size={19} color={palette.muted} />
            </Pressable>
          </View>

          <View style={[styles.divider, { backgroundColor: palette.hairline }]} />

          <ScrollView
            style={styles.scroll}
            contentContainerStyle={styles.form}
            showsVerticalScrollIndicator={false}
          >
            {presets.length > 0 ? (
              <View style={styles.section}>
                <Text style={[styles.label, { color: palette.muted }]}>
                  {t("pluginPresets")}
                </Text>
                <View style={styles.chips}>
                  {presets.map((preset) => {
                    const key = `${preset.plugin_id}:${preset.id}`;
                    return (
                      <SelectableChip
                        key={key}
                        label={preset.name}
                        selected={activePresetId === key}
                        onPress={() => applyPreset(preset)}
                      />
                    );
                  })}
                </View>
              </View>
            ) : null}

            {/* 1. Workspace Directory */}
            <View style={styles.section}>
              <View style={styles.sectionHeader}>
                <Text style={[styles.label, { color: palette.muted }]}>
                  {t("workingFolder")}
                </Text>
                <Pressable
                  onPress={() => setShowPicker(true)}
                  style={styles.browseLink}
                  hitSlop={6}
                >
                  <FolderOpen size={13} color={palette.accent} />
                  <Text style={[styles.browseText, { color: palette.accent }]}>
                    {t("browseFolders")}
                  </Text>
                </Pressable>
              </View>

              <Pressable
                onPress={() => setShowPicker(true)}
                style={[
                  styles.dirRow,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.hairline,
                  },
                ]}
              >
                {selectedDir?.is_git_repo ? (
                  <FolderGit2 size={18} color={palette.accent} />
                ) : (
                  <Folder size={18} color={palette.muted} />
                )}
                <View style={{ flex: 1, minWidth: 0 }}>
                  <View style={styles.dirTitleRow}>
                    <Text
                      numberOfLines={1}
                      style={[styles.dirTitle, { color: palette.text }]}
                    >
                      {selectedDir ? selectedDir.name : t("chooseFolderPrompt")}
                    </Text>
                    {selectedDir?.is_git_repo ? (
                      <View
                        style={[
                          styles.gitBadge,
                          { backgroundColor: palette.accentSoft },
                        ]}
                      >
                        <Text style={[styles.gitText, { color: palette.accent }]}>
                          GIT
                        </Text>
                      </View>
                    ) : null}
                  </View>
                  <Text
                    numberOfLines={1}
                    style={[styles.dirPath, { color: palette.textSecondary }]}
                  >
                    {selectedDir
                      ? selectedDir.display_path
                      : t("tapToBrowseHostFolders")}
                  </Text>
                </View>
                <ChevronRight size={16} color={palette.muted} />
              </Pressable>

              {loadingQuick && quickDirs.length === 0 ? (
                <View style={{ paddingVertical: 4 }}>
                  <ActivityIndicator size="small" color={palette.accent} />
                </View>
              ) : quickDirs.some((d) => d.path !== selectedDir?.path) ? (
                <View style={styles.chips}>
                  {quickDirs
                    .filter((d) => d.path !== selectedDir?.path)
                    .slice(0, 4)
                    .map((dir) => (
                      <SelectableChip
                        key={dir.path}
                        label={dir.name}
                        selected={false}
                        maxLabelWidth={136}
                        onPress={() => handleChooseDir(dir)}
                        icon={(c) =>
                          dir.is_git_repo ? (
                            <FolderGit2 size={13} color={c} />
                          ) : (
                            <Folder size={13} color={c} />
                          )
                        }
                      />
                    ))}
                </View>
              ) : null}
            </View>

            {/* 2. CLI Provider */}
            <View style={styles.section}>
              <View style={styles.sectionHeader}>
                <Text style={[styles.label, { color: palette.muted }]}>
                  {t("agentCli")}
                </Text>
                <Pressable
                  onPress={() => setShowMoreProviders((v) => !v)}
                  hitSlop={6}
                >
                  <Text style={[styles.subAction, { color: palette.muted }]}>
                    {showMoreProviders ? t("hideOtherClis") : t("showOtherClis")}
                  </Text>
                </Pressable>
              </View>
              <View style={styles.chips}>
                {PRIMARY_PROVIDERS.map((p) => (
                  <SelectableChip
                    key={p.id}
                    label={p.label}
                    selected={provider === p.id}
                    onPress={() => handleChooseProvider(p.id)}
                  />
                ))}
              </View>
              {showMoreProviders ? (
                <View style={styles.chips}>
                  {EXTRA_PROVIDERS.map((p) => (
                    <SelectableChip
                      key={p.id}
                      label={p.label}
                      selected={provider === p.id}
                      onPress={() => handleChooseProvider(p.id)}
                    />
                  ))}
                </View>
              ) : null}
            </View>

            {/* 3. Model & Optional Effort/Mode (flat directly on sheet, zero nested cards) */}
            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                {t("modelOptional")}
              </Text>
              <ModelPickerDropdown
                models={providerMeta?.models ?? []}
                selectedModel={selectedModel}
                loading={loadingModels}
                placeholder={t("defaultAuto")}
                onSelect={setSelectedModel}
              />

              {providerMeta &&
              (providerMeta.efforts.length > 0 || providerMeta.modes.length > 0) ? (
                <View style={styles.advancedWrap}>
                  <Pressable
                    onPress={() => setShowAdvanced((v) => !v)}
                    style={[
                      styles.advancedToggle,
                      {
                        backgroundColor: palette.canvas,
                        borderColor: palette.hairline,
                      },
                    ]}
                  >
                    <SlidersHorizontal size={14} color={palette.muted} />
                    <Text
                      numberOfLines={1}
                      style={[styles.advancedTitle, { color: palette.textSecondary }]}
                    >
                      {t("advancedOptions")}
                    </Text>
                    <Text
                      numberOfLines={1}
                      style={[styles.advancedSummary, { color: palette.muted }]}
                    >
                      {selectedEffort || selectedMode
                        ? [
                            selectedEffort ? formatEffortLabel(selectedEffort) : null,
                            selectedMode ? formatModeLabel(selectedMode) : null,
                          ]
                            .filter(Boolean)
                            .join(" · ")
                        : t("advancedOptionsDefault")}
                    </Text>
                    {showAdvanced ? (
                      <ChevronDown size={15} color={palette.muted} />
                    ) : (
                      <ChevronRight size={15} color={palette.muted} />
                    )}
                  </Pressable>

                  {showAdvanced ? (
                    <View style={styles.advancedBody}>
                      {providerMeta.efforts.length > 0 ? (
                        <View style={styles.inlineOptionGroup}>
                          <Text style={[styles.subLabel, { color: palette.muted }]}>
                            {t("optionsReasoningEffort")}
                          </Text>
                          <View style={styles.chips}>
                            <SelectableChip
                              label={t("defaultOption")}
                              selected={selectedEffort === ""}
                              onPress={() => setSelectedEffort("")}
                            />
                            {providerMeta.efforts.map((ef) => (
                              <SelectableChip
                                key={ef}
                                label={formatEffortLabel(ef)}
                                selected={selectedEffort === ef}
                                onPress={() =>
                                  setSelectedEffort(selectedEffort === ef ? "" : ef)
                                }
                              />
                            ))}
                          </View>
                        </View>
                      ) : null}

                      {providerMeta.modes.length > 0 ? (
                        <View style={styles.inlineOptionGroup}>
                          <Text style={[styles.subLabel, { color: palette.muted }]}>
                            {t("optionsExecutionMode")}
                          </Text>
                          <View style={styles.chips}>
                            <SelectableChip
                              label={t("defaultOption")}
                              selected={selectedMode === ""}
                              onPress={() => setSelectedMode("")}
                            />
                            {providerMeta.modes.map((md) => (
                              <SelectableChip
                                key={md}
                                label={formatModeLabel(md)}
                                selected={selectedMode === md}
                                onPress={() =>
                                  setSelectedMode(selectedMode === md ? "" : md)
                                }
                              />
                            ))}
                          </View>
                        </View>
                      ) : null}
                    </View>
                  ) : null}
                </View>
              ) : null}
            </View>

            {/* 4. Session name & optional Git worktree */}
            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                {t("sessionName")}
              </Text>
              <TextInput
                value={name}
                onChangeText={(v) => {
                  setName(v);
                  setNameEdited(true);
                }}
                placeholder="agy-mobile"
                placeholderTextColor={palette.muted}
                autoCapitalize="none"
                style={[
                  styles.input,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.hairline,
                    color: palette.text,
                  },
                ]}
              />
            </View>

            <View style={styles.switchRow}>
              <View style={[styles.switchLabel, { flex: 1, paddingRight: 10 }]}>
                <Shield
                  size={15}
                  color={sandboxEnabled ? palette.success : palette.warning}
                />
                <View style={{ flex: 1 }}>
                  <Text style={[styles.switchText, { color: palette.text }]}>
                    {t("sandboxToggleLabel")}
                  </Text>
                  <Text style={[styles.subLabel, { color: palette.muted }]}>
                    {t("sandboxToggleDesc")}
                  </Text>
                </View>
              </View>
              <CapsuleSwitch
                value={sandboxEnabled}
                onValueChange={setSandboxEnabled}
                accessibilityLabel={t("sandboxToggleLabel")}
              />
            </View>

            {selectedDir?.is_git_repo ? (
              <View style={styles.switchRow}>
                <View style={styles.switchLabel}>
                  <GitBranch size={15} color={palette.accent} />
                  <Text style={[styles.switchText, { color: palette.text }]}>
                    {t("startInNewWorktree")}
                  </Text>
                </View>
                <CapsuleSwitch
                  value={createWorktree}
                  onValueChange={setCreateWorktree}
                  accessibilityLabel={t("startInNewWorktree")}
                />
              </View>
            ) : null}

            {createWorktree && selectedDir?.is_git_repo ? (
              <View style={styles.section}>
                <Text style={[styles.label, { color: palette.muted }]}>
                  {t("newBranchName")}
                </Text>
                <TextInput
                  value={branch}
                  onChangeText={setBranch}
                  placeholder="feat/mobile-task"
                  placeholderTextColor={palette.muted}
                  autoCapitalize="none"
                  style={[
                    styles.input,
                    {
                      backgroundColor: palette.canvas,
                      borderColor: palette.borderSubtle,
                      color: palette.text,
                    },
                  ]}
                />
              </View>
            ) : null}

            {error ? (
              <Text style={[styles.errorText, { color: palette.danger }]}>
                {error}
              </Text>
            ) : null}
          </ScrollView>

          {/* Anchored sticky footer with the single solid Desk Teal primary CTA */}
          <View
            style={[
              styles.footer,
              {
                backgroundColor: palette.panel,
                borderTopColor: palette.hairline,
                paddingBottom: Math.max(insets.bottom, 16) + 8,
              },
            ]}
          >
            <Pressable
              onPress={() => void launch()}
              disabled={busy}
              style={[
                styles.submit,
                { backgroundColor: palette.accent, opacity: busy ? 0.7 : 1 },
              ]}
            >
              {busy ? (
                <ActivityIndicator size="small" color={palette.accentText} />
              ) : (
                <>
                  <Play size={15} color={palette.accentText} fill={palette.accentText} />
                  <Text style={[styles.submitText, { color: palette.accentText }]}>
                    {t("startProvider", { provider: providerLabel })}
                  </Text>
                </>
              )}
            </Pressable>
          </View>
        </View>
      </View>

      <DirectoryPickerModal
        visible={showPicker}
        hostId={hostId}
        initialPath={selectedDir?.path}
        onClose={() => setShowPicker(false)}
        onSelect={handleChooseDir}
      />
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: {
    flex: 1,
    backgroundColor: "rgba(4,7,11,0.72)",
    justifyContent: "flex-end",
  },
  sheet: {
    borderTopLeftRadius: 20,
    borderTopRightRadius: 20,
    borderTopWidth: 1,
    borderLeftWidth: 1,
    borderRightWidth: 1,
    paddingTop: 10,
    maxHeight: "88%",
  },
  grabBar: {
    width: 36,
    height: 4,
    borderRadius: 2,
    alignSelf: "center",
    marginBottom: 10,
  },
  header: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    paddingHorizontal: 16,
    paddingBottom: 10,
  },
  title: {
    fontFamily: FONTS.bold,
    fontSize: 16.5,
    letterSpacing: -0.2,
  },
  closeBtn: {
    width: 32,
    height: 32,
    borderRadius: 8,
    alignItems: "center",
    justifyContent: "center",
  },
  divider: {
    height: 1,
  },
  scroll: {
    flexGrow: 0,
  },
  form: {
    paddingHorizontal: 16,
    paddingTop: 14,
    paddingBottom: 14,
    gap: 14,
  },
  section: { gap: 7 },
  sectionHeader: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
  },
  label: {
    fontFamily: FONTS.regular,
    fontSize: 11.5,
    letterSpacing: 0.2,
  },
  subLabel: {
    fontFamily: FONTS.regular,
    fontSize: 11.5,
    letterSpacing: 0.2,
  },
  subAction: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
  browseLink: { flexDirection: "row", alignItems: "center", gap: 4 },
  browseText: {
    fontFamily: FONTS.semi,
    fontSize: 12,
  },
  dirRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 10,
    borderWidth: 1,
    borderRadius: 11,
    paddingHorizontal: 12,
    paddingVertical: 10,
  },
  dirTitleRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  dirTitle: {
    fontFamily: FONTS.bold,
    fontSize: 14.5,
    flexShrink: 1,
  },
  dirPath: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
    marginTop: 2,
  },
  gitBadge: {
    borderRadius: 5,
    paddingHorizontal: 5,
    paddingVertical: 1,
  },
  gitText: {
    fontFamily: FONTS.semi,
    fontSize: 10,
    letterSpacing: 0.3,
  },
  quickScrollWrap: {
    position: "relative",
    marginHorizontal: -16,
  },
  bleedScroll: {},
  quickRow: {
    paddingLeft: 16,
    paddingRight: 58,
    gap: 8,
    paddingVertical: 2,
  },
  quickFade: {
    position: "absolute",
    top: 0,
    right: 0,
    bottom: 0,
    width: 60,
  },
  chips: { flexDirection: "row", flexWrap: "wrap", gap: 8 },
  advancedWrap: {
    marginTop: 4,
    gap: 10,
  },
  advancedToggle: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
    height: 42,
    paddingHorizontal: 12,
    borderRadius: 10,
    borderWidth: 1,
  },
  advancedTitle: {
    fontFamily: FONTS.semi,
    fontSize: 13,
  },
  advancedSummary: {
    flex: 1,
    fontFamily: FONTS.regular,
    fontSize: 12,
    textAlign: "right",
  },
  advancedBody: {
    gap: 10,
    paddingTop: 2,
  },
  inlineOptionGroup: {
    gap: 7,
  },
  input: {
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 12,
    height: 44,
    fontFamily: FONTS.regular,
    fontSize: 14.5,
  },
  switchRow: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    paddingVertical: 4,
  },
  switchLabel: {
    flexDirection: "row",
    alignItems: "center",
    gap: 7,
  },
  switchText: {
    fontFamily: FONTS.regular,
    fontSize: 13.5,
  },
  errorText: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  footer: {
    borderTopWidth: 1,
    paddingHorizontal: 16,
    paddingTop: 10,
  },
  submit: {
    height: 46,
    borderRadius: 12,
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 8,
  },
  submitText: {
    fontFamily: FONTS.bold,
    fontSize: 14.5,
  },
});

import React, { useState } from "react";
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
import { Sparkles, Wand2, X } from "lucide-react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { SelectableChip } from "./SelectableChip";

interface PluginCreatorSheetProps {
  visible: boolean;
  hostId: string;
  onClose: () => void;
  onLaunched?: () => void;
}

const CREATOR_PROVIDERS = [
  { id: "agy", label: "Antigravity (agy)" },
  { id: "claude", label: "Claude Code" },
  { id: "codex", label: "Codex" },
] as const;

const STARTER_IDEAS = [
  {
    label: "Tokyo Night 테마",
    goal: "Tokyo Night 다크 테마 플러그인을 만들어서 바로 적용해줘.",
  },
  {
    label: "PR·테스트 현황 패널",
    goal: "현재 프로젝트의 git 브랜치, 변경 파일 수, 최근 커밋 5개와 테스트 실행 버튼을 보여주는 워크스페이스 패널 플러그인을 만들어줘.",
  },
  {
    label: "/standup 요약 커맨드",
    goal: "오늘 커밋과 변경 내역을 요약해 타임라인 카드로 올려주는 /standup 슬래시 커맨드 플러그인을 만들어줘.",
  },
  {
    label: "TODO 체크리스트 패널",
    goal: "작업 폴더별 TODO 항목을 추가하고 완료 토글할 수 있는 인터랙티브 패널과 /todo 커맨드를 만들어줘.",
  },
];

export function PluginCreatorSheet({
  visible,
  hostId,
  onClose,
  onLaunched,
}: PluginCreatorSheetProps) {
  const { palette } = useMasilTheme();
  const insets = useSafeAreaInsets();
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const selectAgent = useHostRuntimeStore((s) => s.selectAgent);
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);

  const [provider, setProvider] = useState<"agy" | "claude" | "codex">("agy");
  const [goal, setGoal] = useState("");
  const [pluginId, setPluginId] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const launchCreator = async () => {
    const trimmed = goal.trim();
    if (!trimmed) {
      setError("만들고 싶은 플러그인 기능을 적어주세요.");
      return;
    }
    const client = getHostClient(hostId);
    if (!client) {
      setError("호스트에 연결되어 있지 않습니다.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const res = await client.request<{
        agent_name?: string;
        started?: { pane?: string; pane_id?: string; name?: string };
      }>("plugin.creator.start.request", {
        goal: trimmed,
        provider,
        plugin_id: pluginId.trim() || undefined,
      });
      await refreshHost(hostId, true);
      setTimeout(() => {
        void useHostRuntimeStore.getState().refreshHost(hostId, false);
      }, 1500);

      const latestHost = useHostRuntimeStore.getState().hosts[hostId];
      const paneHint = res.started?.pane_id ?? res.started?.pane;
      const found = latestHost?.agents.find(
        (a) =>
          (paneHint && a.pane === paneHint) ||
          a.name === (res.started?.name ?? res.agent_name)
      );
      if (found) {
        selectAgent(hostId, found.remote_id);
        selectPanel("agent");
      }
      setGoal("");
      setPluginId("");
      onClose();
      onLaunched?.();
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
            {
              backgroundColor: palette.panel,
              borderColor: palette.borderSubtle,
              paddingBottom: Math.max(insets.bottom, 16),
            },
          ]}
        >
          <View style={[styles.grabBar, { backgroundColor: palette.track }]} />
          <View style={styles.header}>
            <View style={styles.titleRow}>
              <Sparkles size={17} color={palette.accent} />
              <Text style={[styles.title, { color: palette.text }]}>
                AI로 플러그인 만들기
              </Text>
            </View>
            <Pressable onPress={onClose} hitSlop={8}>
              <X size={19} color={palette.muted} />
            </Pressable>
          </View>
          <View
            style={[styles.divider, { backgroundColor: palette.hairline }]}
          />

          <ScrollView contentContainerStyle={styles.body}>
            <Text style={[styles.sub, { color: palette.textSecondary }]}>
              선택한 AI CLI에 공식 masil-plugin 스킬을 자동으로 설치하고,
              플러그인 생성·검증·실시간 반영까지 한 번에 진행합니다.
            </Text>

            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                제작에 사용할 AI 도구
              </Text>
              <View style={styles.chips}>
                {CREATOR_PROVIDERS.map((p) => (
                  <SelectableChip
                    key={p.id}
                    label={p.label}
                    selected={provider === p.id}
                    onPress={() => setProvider(p.id)}
                  />
                ))}
              </View>
            </View>

            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                빠른 아이디어
              </Text>
              <View style={styles.chips}>
                {STARTER_IDEAS.map((idea) => (
                  <SelectableChip
                    key={idea.label}
                    label={idea.label}
                    selected={goal === idea.goal}
                    onPress={() => setGoal(idea.goal)}
                  />
                ))}
              </View>
            </View>

            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                어떤 플러그인을 만들까요?
              </Text>
              <TextInput
                value={goal}
                onChangeText={setGoal}
                multiline
                placeholder="예: GitHub PR 상태와 최근 CI 결과를 보여주는 패널 만들어줘"
                placeholderTextColor={palette.muted}
                style={[
                  styles.textarea,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.borderSubtle,
                    color: palette.text,
                  },
                ]}
              />
            </View>

            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                플러그인 ID (선택, 영문 소문자/하이픈)
              </Text>
              <TextInput
                value={pluginId}
                onChangeText={setPluginId}
                autoCapitalize="none"
                autoCorrect={false}
                placeholder="자동 생성 (예: pr-radar)"
                placeholderTextColor={palette.muted}
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

            {error ? (
              <Text style={[styles.err, { color: palette.danger }]}>
                {error}
              </Text>
            ) : null}

            <Pressable
              onPress={() => void launchCreator()}
              disabled={busy}
              style={[
                styles.launchBtn,
                {
                  backgroundColor: palette.accent,
                  opacity: busy ? 0.65 : 1,
                },
              ]}
            >
              {busy ? (
                <ActivityIndicator size="small" color={palette.accentText} />
              ) : (
                <>
                  <Wand2 size={16} color={palette.accentText} />
                  <Text style={[styles.launchText, { color: palette.accentText }]}>
                    AI 제작 세션 시작
                  </Text>
                </>
              )}
            </Pressable>
          </ScrollView>
        </View>
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: {
    flex: 1,
    backgroundColor: "rgba(0,0,0,0.58)",
    justifyContent: "flex-end",
  },
  sheet: {
    maxHeight: "88%",
    borderTopLeftRadius: 20,
    borderTopRightRadius: 20,
    borderTopWidth: 1,
  },
  grabBar: {
    width: 36,
    height: 4,
    borderRadius: 2,
    alignSelf: "center",
    marginTop: 9,
    marginBottom: 4,
  },
  header: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    paddingHorizontal: 16,
    paddingVertical: 12,
  },
  titleRow: { flexDirection: "row", alignItems: "center", gap: 8 },
  title: { fontFamily: FONTS.bold, fontSize: 16 },
  divider: { height: 1 },
  body: { padding: 16, gap: 14 },
  sub: { fontFamily: FONTS.regular, fontSize: 13, lineHeight: 19 },
  section: { gap: 7 },
  label: { fontFamily: FONTS.semi, fontSize: 12 },
  chips: { flexDirection: "row", flexWrap: "wrap", gap: 7 },
  textarea: {
    minHeight: 88,
    borderWidth: 1,
    borderRadius: 11,
    paddingHorizontal: 12,
    paddingVertical: 10,
    fontFamily: FONTS.regular,
    fontSize: 14,
    lineHeight: 20,
    textAlignVertical: "top",
  },
  input: {
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 12,
    paddingVertical: 9,
    fontFamily: FONTS.mono,
    fontSize: 13,
  },
  err: { fontFamily: FONTS.regular, fontSize: 12.5 },
  launchBtn: {
    height: 44,
    borderRadius: 11,
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 8,
    marginTop: 4,
  },
  launchText: { fontFamily: FONTS.bold, fontSize: 14.5 },
});

import React, { useMemo, useState } from "react";
import {
  ActivityIndicator,
  FlatList,
  Modal,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { Check, ChevronDown, Search, Sparkles, X } from "lucide-react-native";
import type { ProviderModelOption } from "../protocol/schemas";
import { matchesMasilQuery } from "../search/korean-choseong";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";

interface ModelPickerDropdownProps {
  models: ProviderModelOption[];
  selectedModel: string;
  currentModel?: string | null;
  loading?: boolean;
  placeholder?: string;
  onSelect: (modelId: string) => void;
}

export function ModelPickerDropdown({
  models,
  selectedModel,
  currentModel,
  loading = false,
  placeholder = "기본값 (자동)",
  onSelect,
}: ModelPickerDropdownProps) {
  const { palette } = useMasilTheme();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [customDraft, setCustomDraft] = useState("");

  const selectedLabel = useMemo(() => {
    if (!selectedModel) {
      return currentModel ? `현재: ${currentModel}` : placeholder;
    }
    const found = models.find((m) => m.id === selectedModel);
    return found ? found.label : selectedModel;
  }, [models, selectedModel, currentModel, placeholder]);

  const filtered = useMemo(() => {
    if (!query.trim()) return models;
    return models.filter((m) => matchesMasilQuery(`${m.label} ${m.id}`, query));
  }, [models, query]);

  const choose = (id: string) => {
    onSelect(id);
    setOpen(false);
    setQuery("");
  };

  const hasCustom = customDraft.trim().length > 0;

  return (
    <>
      <Pressable
        onPress={() => {
          setCustomDraft("");
          setQuery("");
          setOpen(true);
        }}
        style={[
          styles.trigger,
          {
            backgroundColor: palette.canvas,
            borderColor: selectedModel
              ? palette.accentBorder
              : palette.hairline,
          },
        ]}
      >
        <Sparkles size={15} color={palette.accent} />
        <View style={{ flex: 1 }}>
          <Text
            numberOfLines={1}
            style={{
              color:
                selectedModel || currentModel ? palette.text : palette.muted,
              fontFamily: selectedModel ? FONTS.semi : FONTS.regular,
              fontSize: 13.5,
            }}
          >
            {selectedLabel}
          </Text>
          {selectedModel ? (
            <Text
              numberOfLines={1}
              style={{
                color: palette.muted,
                fontSize: 11,
                fontFamily: FONTS.mono,
                marginTop: 1,
              }}
            >
              {selectedModel}
            </Text>
          ) : null}
        </View>
        {loading ? (
          <ActivityIndicator size="small" color={palette.accent} />
        ) : (
          <ChevronDown size={16} color={palette.muted} />
        )}
      </Pressable>

      <Modal
        visible={open}
        transparent
        animationType="slide"
        onRequestClose={() => setOpen(false)}
      >
        <View style={styles.backdrop}>
          <View
            style={[
              styles.sheet,
              { backgroundColor: palette.canvas, borderColor: palette.borderSubtle },
            ]}
          >
            <View style={[styles.grabBar, { backgroundColor: palette.track }]} />
            <View
              style={[
                styles.header,
                { backgroundColor: palette.canvas, borderBottomColor: palette.hairline },
              ]}
            >
              <Text style={[styles.title, { color: palette.text }]}>
                모델 선택
              </Text>
              <Pressable onPress={() => setOpen(false)} hitSlop={8}>
                <X size={19} color={palette.muted} />
              </Pressable>
            </View>

            <View style={styles.searchWrap}>
              <View
                style={[
                  styles.searchBox,
                  {
                    backgroundColor: palette.panel,
                    borderColor: palette.borderSubtle,
                  },
                ]}
              >
                <Search size={14} color={palette.muted} />
                <View style={{ flex: 1, justifyContent: "center", position: "relative" }}>
                  {query.length === 0 ? (
                    <Text
                      pointerEvents="none"
                      numberOfLines={1}
                      style={{
                        position: "absolute",
                        left: 0,
                        right: 0,
                        fontFamily: FONTS.regular,
                        fontSize: 13.5,
                        color: palette.muted,
                      }}
                    >
                      모델 검색 (예: neon, opus, sonnet, gpt)
                    </Text>
                  ) : null}
                  <TextInput
                    value={query}
                    onChangeText={setQuery}
                    autoCapitalize="none"
                    autoCorrect={false}
                    style={[styles.searchInput, { color: palette.text }]}
                  />
                </View>
                {query.length > 0 ? (
                  <Pressable onPress={() => setQuery("")} hitSlop={6}>
                    <X size={14} color={palette.muted} />
                  </Pressable>
                ) : null}
              </View>
            </View>

            <FlatList
              data={filtered}
              keyExtractor={(item) => item.id}
              contentContainerStyle={styles.listContent}
              ListHeaderComponent={
                <Pressable
                  onPress={() => choose("")}
                  style={[
                    styles.optionRow,
                    {
                      backgroundColor: !selectedModel
                        ? palette.accentSoft
                        : palette.panel,
                      borderColor: !selectedModel
                        ? palette.accentBorder
                        : palette.hairline,
                    },
                  ]}
                >
                  <View style={{ flex: 1 }}>
                    <Text
                      style={{
                        color: palette.text,
                        fontFamily: FONTS.semi,
                        fontSize: 13.5,
                      }}
                    >
                      {placeholder}
                    </Text>
                    <Text
                      style={{
                        color: palette.muted,
                        fontFamily: FONTS.regular,
                        fontSize: 11.5,
                        marginTop: 2,
                      }}
                    >
                      CLI 기본 설정 모델을 사용합니다
                    </Text>
                  </View>
                  {!selectedModel ? (
                    <Check size={16} color={palette.accent} />
                  ) : null}
                </Pressable>
              }
              ListEmptyComponent={
                <Text
                  style={{
                    color: palette.muted,
                    fontFamily: FONTS.regular,
                    fontSize: 12.5,
                    textAlign: "center",
                    paddingVertical: 20,
                  }}
                >
                  {loading
                    ? "호스트에서 모델 목록을 불러오는 중..."
                    : "일치하는 모델이 없습니다. 아래에서 직접 입력할 수 있습니다."}
                </Text>
              }
              renderItem={({ item }) => {
                const isSelected = selectedModel === item.id;
                const isActive = currentModel === item.id;
                return (
                  <Pressable
                    onPress={() => choose(item.id)}
                    style={[
                      styles.optionRow,
                      {
                        backgroundColor: isSelected
                          ? palette.accentSoft
                          : palette.panel,
                        borderColor: isSelected
                          ? palette.accentBorder
                          : isActive
                            ? palette.success
                            : palette.hairline,
                      },
                    ]}
                  >
                    <View style={{ flex: 1 }}>
                      <View style={{ flexDirection: "row", alignItems: "center", gap: 6 }}>
                        <Text
                          numberOfLines={1}
                          style={{
                            color: palette.text,
                            fontFamily: FONTS.semi,
                            fontSize: 13.5,
                            flexShrink: 1,
                          }}
                        >
                          {item.label}
                        </Text>
                        {isActive ? (
                          <View
                            style={[
                              styles.activeBadge,
                              { backgroundColor: palette.successSoft },
                            ]}
                          >
                            <Text
                              style={{
                                color: palette.success,
                                fontFamily: FONTS.semi,
                                fontSize: 10.5,
                              }}
                            >
                              현재
                            </Text>
                          </View>
                        ) : null}
                      </View>
                      <Text
                        numberOfLines={1}
                        style={{
                          color: palette.muted,
                          fontSize: 11.5,
                          fontFamily: FONTS.mono,
                          marginTop: 2,
                        }}
                      >
                        {item.id}
                      </Text>
                    </View>
                    {isSelected ? (
                      <Check size={16} color={palette.accent} />
                    ) : null}
                  </Pressable>
                );
              }}
            />

            <View
              style={[
                styles.customFooter,
                { backgroundColor: palette.panel, borderTopColor: palette.hairline },
              ]}
            >
              <TextInput
                value={customDraft}
                onChangeText={setCustomDraft}
                placeholder="직접 모델 ID 입력..."
                placeholderTextColor={palette.muted}
                autoCapitalize="none"
                autoCorrect={false}
                onSubmitEditing={() => {
                  if (hasCustom) choose(customDraft.trim());
                }}
                style={[
                  styles.customInput,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.borderSubtle,
                    color: palette.text,
                  },
                ]}
              />
              <Pressable
                onPress={() => {
                  if (hasCustom) choose(customDraft.trim());
                }}
                disabled={!hasCustom}
                style={[
                  styles.customBtn,
                  {
                    backgroundColor: hasCustom
                      ? palette.accent
                      : palette.disabledBg,
                    borderWidth: hasCustom ? 0 : 1,
                    borderColor: palette.borderSubtle,
                  },
                ]}
              >
                <Text
                  style={{
                    color: hasCustom ? palette.accentText : palette.disabledText,
                    fontFamily: FONTS.bold,
                    fontSize: 12.5,
                  }}
                >
                  적용
                </Text>
              </Pressable>
            </View>
          </View>
        </View>
      </Modal>
    </>
  );
}

const styles = StyleSheet.create({
  trigger: {
    flexDirection: "row",
    alignItems: "center",
    gap: 10,
    borderWidth: 1,
    borderRadius: 11,
    paddingHorizontal: 12,
    height: 44,
  },
  backdrop: {
    flex: 1,
    backgroundColor: "rgba(0,0,0,0.65)",
    justifyContent: "flex-end",
  },
  sheet: {
    height: "78%",
    borderTopLeftRadius: 20,
    borderTopRightRadius: 20,
    borderTopWidth: 1,
    borderLeftWidth: 1,
    borderRightWidth: 1,
    overflow: "hidden",
  },
  grabBar: {
    width: 36,
    height: 4,
    borderRadius: 2,
    alignSelf: "center",
    marginTop: 10,
  },
  header: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    paddingHorizontal: 16,
    paddingVertical: 12,
    borderBottomWidth: 1,
  },
  title: {
    fontFamily: FONTS.bold,
    fontSize: 16,
    letterSpacing: -0.2,
  },
  searchWrap: {
    paddingHorizontal: 14,
    paddingVertical: 10,
  },
  searchBox: {
    flexDirection: "row",
    alignItems: "center",
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 11,
    height: 40,
    gap: 8,
  },
  searchInput: {
    flex: 1,
    paddingVertical: 0,
    fontFamily: FONTS.regular,
    fontSize: 13.5,
  },
  listContent: {
    paddingHorizontal: 14,
    paddingBottom: 16,
    gap: 7,
  },
  optionRow: {
    flexDirection: "row",
    alignItems: "center",
    borderWidth: 1,
    borderRadius: 11,
    paddingHorizontal: 13,
    paddingVertical: 11,
    gap: 10,
  },
  activeBadge: {
    paddingHorizontal: 6,
    paddingVertical: 1,
    borderRadius: 999,
  },
  customFooter: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 14,
    paddingVertical: 10,
    borderTopWidth: 1,
    gap: 8,
  },
  customInput: {
    flex: 1,
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 11,
    height: 40,
    fontSize: 12.5,
    fontFamily: FONTS.mono,
  },
  customBtn: {
    paddingHorizontal: 16,
    height: 40,
    borderRadius: 10,
    alignItems: "center",
    justifyContent: "center",
  },
});

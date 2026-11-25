import React from "react";
import { Pressable, ScrollView, StyleSheet, Text, View } from "react-native";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS, TABULAR_NUMS } from "../theme/typography";

export interface SegmentOption<T extends string> {
  id: T;
  label: string;
  count?: number;
  icon?: (color: string, active: boolean) => React.ReactNode;
}

interface SegmentedTabsProps<T extends string> {
  options: Array<SegmentOption<T>>;
  value: T;
  onChange: (value: T) => void;
  scrollable?: boolean;
}

/**
 * Compact 36px Linear-style segmented control with a recessed track and raised
 * active thumb so navigation tabs never compete with the primary CTA fill.
 * Automatically switches to horizontal scrolling when dynamic plugin tabs are added.
 */
export function SegmentedTabs<T extends string>({
  options,
  value,
  onChange,
  scrollable,
}: SegmentedTabsProps<T>) {
  const { palette } = useMasilTheme();
  const useScroll = scrollable ?? options.length > 4;

  const renderTabs = () =>
    options.map((opt) => {
      const active = opt.id === value;
      const iconColor = active ? palette.accent : palette.muted;
      return (
        <Pressable
          key={opt.id}
          onPress={() => onChange(opt.id)}
          style={[
            styles.thumb,
            useScroll ? styles.thumbScroll : styles.thumbFlex,
            active && {
              backgroundColor: palette.segmentThumb,
              borderColor: palette.borderSubtle,
            },
          ]}
        >
          {opt.icon ? opt.icon(iconColor, active) : null}
          <Text
            numberOfLines={1}
            style={[
              styles.label,
              {
                color: active ? palette.text : palette.muted,
                fontFamily: active ? FONTS.semi : FONTS.regular,
              },
            ]}
          >
            {opt.label}
          </Text>
          {typeof opt.count === "number" ? (
            <Text
              style={[
                styles.count,
                {
                  color: active ? palette.accent : palette.muted,
                  backgroundColor: active
                    ? palette.accentSoft
                    : "transparent",
                },
              ]}
            >
              {opt.count}
            </Text>
          ) : null}
        </Pressable>
      );
    });

  if (useScroll) {
    return (
      <View
        style={[
          styles.track,
          {
            backgroundColor: palette.segmentTrack,
            borderColor: palette.hairline,
          },
        ]}
      >
        <ScrollView
          horizontal
          showsHorizontalScrollIndicator={false}
          contentContainerStyle={styles.scrollContent}
        >
          {renderTabs()}
        </ScrollView>
      </View>
    );
  }

  return (
    <View
      style={[
        styles.track,
        {
          backgroundColor: palette.segmentTrack,
          borderColor: palette.hairline,
        },
      ]}
    >
      {renderTabs()}
    </View>
  );
}

const styles = StyleSheet.create({
  track: {
    flexDirection: "row",
    alignItems: "center",
    borderRadius: 10,
    borderWidth: 1,
    padding: 3,
    gap: 3,
    height: 38,
  },
  scrollContent: {
    flexDirection: "row",
    alignItems: "center",
    gap: 3,
  },
  thumb: {
    height: 30,
    borderRadius: 7,
    borderWidth: 1,
    borderColor: "transparent",
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 5,
    paddingHorizontal: 8,
  },
  thumbFlex: {
    flex: 1,
  },
  thumbScroll: {
    paddingHorizontal: 11,
  },
  label: {
    fontSize: 12.5,
    letterSpacing: -0.1,
  },
  count: {
    fontFamily: FONTS.semi,
    fontVariant: TABULAR_NUMS,
    fontSize: 11.5,
    paddingHorizontal: 5,
    paddingVertical: 1,
    borderRadius: 999,
    overflow: "hidden",
  },
});

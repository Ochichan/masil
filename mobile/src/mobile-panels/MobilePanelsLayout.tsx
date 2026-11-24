import React, { useEffect } from "react";
import { StyleSheet, View, useWindowDimensions } from "react-native";
import { Gesture, GestureDetector } from "react-native-gesture-handler";
import Animated, {
  runOnJS,
  useAnimatedStyle,
  useSharedValue,
  withSpring,
} from "react-native-reanimated";
import {
  MobilePanelTarget,
  positionToTarget,
  targetToPosition,
  useMobilePanelsStore,
} from "./store";

interface MobilePanelsLayoutProps {
  left: React.ReactNode;
  center: React.ReactNode;
  right: React.ReactNode;
  backdropColor: string;
}

const SPRING_CONFIG = {
  damping: 26,
  stiffness: 260,
  mass: 0.9,
};

export function MobilePanelsLayout({
  left,
  center,
  right,
  backdropColor,
}: MobilePanelsLayoutProps) {
  const { width } = useWindowDimensions();
  const target = useMobilePanelsStore((s) => s.target);
  const revision = useMobilePanelsStore((s) => s.revision);
  const openBlockers = useMobilePanelsStore((s) => s.openGestureBlockers);
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);
  const settleAt = useMobilePanelsStore((s) => s.settleAt);

  // Single normalized position: -1 (left), 0 (center), +1 (right).
  const position = useSharedValue(0);
  const dragStartPos = useSharedValue(0);

  useEffect(() => {
    const dest = targetToPosition(target);
    position.value = withSpring(dest, SPRING_CONFIG, (finished) => {
      if (finished) {
        runOnJS(settleAt)(target, revision);
      }
    });
  }, [target, revision, position, settleAt]);

  const commitTarget = (next: MobilePanelTarget) => {
    selectPanel(next);
  };

  const pan = Gesture.Pan()
    .activeOffsetX([-18, 18])
    .failOffsetY([-14, 14])
    .onStart(() => {
      dragStartPos.value = position.value;
    })
    .onUpdate((e) => {
      // When a terminal pan blocker is active, allow closing an open side panel
      // but block opening a side panel from center.
      if (openBlockers > 0 && Math.abs(dragStartPos.value) < 0.05) {
        return;
      }
      const delta = -e.translationX / Math.max(width, 1);
      const next = Math.max(-1, Math.min(1, dragStartPos.value + delta));
      position.value = next;
    })
    .onEnd((e) => {
      if (openBlockers > 0 && Math.abs(dragStartPos.value) < 0.05) {
        return;
      }
      const velocityBias =
        e.velocityX > 550 ? -0.45 : e.velocityX < -550 ? 0.45 : 0;
      const projected = Math.max(
        -1,
        Math.min(1, position.value + velocityBias)
      );
      const nextTarget = positionToTarget(projected);
      runOnJS(commitTarget)(nextTarget);
    });

  const leftStyle = useAnimatedStyle(() => ({
    transform: [{ translateX: (-1 - position.value) * width }],
  }));

  const rightStyle = useAnimatedStyle(() => ({
    transform: [{ translateX: (1 - position.value) * width }],
  }));

  const backdropStyle = useAnimatedStyle(() => ({
    opacity: Math.min(0.65, Math.abs(position.value) * 0.65),
  }));

  return (
    <GestureDetector gesture={pan}>
      <View style={styles.root}>
        <View style={styles.fill}>{center}</View>

        <Animated.View
          pointerEvents="none"
          style={[
            styles.fill,
            { backgroundColor: backdropColor },
            backdropStyle,
          ]}
        />

        <Animated.View
          style={[styles.overlay, { width }, leftStyle]}
          pointerEvents={target === "agent-list" ? "auto" : "none"}
        >
          {left}
        </Animated.View>

        <Animated.View
          style={[styles.overlay, { width }, rightStyle]}
          pointerEvents={target === "workspace-tools" ? "auto" : "none"}
        >
          {right}
        </Animated.View>
      </View>
    </GestureDetector>
  );
}

const styles = StyleSheet.create({
  root: {
    flex: 1,
    overflow: "hidden",
  },
  fill: {
    position: "absolute",
    top: 0,
    right: 0,
    bottom: 0,
    left: 0,
  },
  overlay: {
    position: "absolute",
    top: 0,
    bottom: 0,
    left: 0,
  },
});

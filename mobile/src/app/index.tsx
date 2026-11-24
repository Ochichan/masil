import React from "react";
import { useHostRuntimeStore } from "../hosts/HostRuntimeController";
import { MobilePanelsLayout } from "../mobile-panels/MobilePanelsLayout";
import { AgentDetailScreen } from "../screens/AgentDetailScreen";
import { AgentListPanel } from "../screens/AgentListPanel";
import { WorkspaceToolsPanel } from "../screens/WorkspaceToolsPanel";
import { useMasilTheme } from "../theme/ThemeProvider";

export default function MainWorkspaceScreen() {
  const { palette } = useMasilTheme();
  const hosts = useHostRuntimeStore((s) => s.hosts);
  const activeHostId = useHostRuntimeStore((s) => s.activeHostId);
  const activeAgentRemoteId = useHostRuntimeStore((s) => s.activeAgentRemoteId);

  const activeHost = activeHostId ? hosts[activeHostId] : undefined;
  const activeAgent =
    activeHost?.agents.find((a) => a.remote_id === activeAgentRemoteId) ??
    activeHost?.agents[0] ??
    null;

  return (
    <MobilePanelsLayout
      backdropColor={palette.canvas}
      left={<AgentListPanel />}
      center={<AgentDetailScreen hostId={activeHostId} agent={activeAgent} />}
      right={<WorkspaceToolsPanel hostId={activeHostId} agent={activeAgent} />}
    />
  );
}

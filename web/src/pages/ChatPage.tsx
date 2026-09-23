import { useCallback, useMemo, useState } from "react";

import { ChatHeader } from "../components/ChatHeader";
import { useAuth } from "../app/AuthProvider";
import { Composer } from "../components/Composer";
import { MemoryPanel } from "../components/MemoryPanel";
import { MessageList } from "../components/MessageList";
import { SessionSidebar } from "../components/SessionSidebar";
import { SessionCreationDialog } from "../components/SessionCreationDialog";
import { useChatController } from "../features/chat/useChatController";
import { useTheme } from "../features/theme/useTheme";
import type { WorkflowMode } from "../api/types";

export function ChatPage() {
  const auth = useAuth();
  const chat = useChatController();
  const { theme, toggleTheme } = useTheme();
  const [sessionsOpen, setSessionsOpen] = useState(false);
  const [sessionsCollapsed, setSessionsCollapsed] = useState(false);
  const [creationDialogOpen, setCreationDialogOpen] = useState(false);
  const [creatingSession, setCreatingSession] = useState(false);
  const closeCreationDialog = useCallback(() => setCreationDialogOpen(false), []);
  const [memoryOpen, setMemoryOpen] = useState(() =>
    window.matchMedia("(min-width: 1101px)").matches,
  );
  const selectedSession = useMemo(
    () => chat.sessions.find((session) => session.id === chat.selectedId) ?? null,
    [chat.selectedId, chat.sessions],
  );

  return (
    <main className={`app-shell ${memoryOpen ? "memory-visible" : ""} ${sessionsCollapsed ? "sessions-collapsed" : ""}`}>
      <SessionSidebar
        sessions={chat.sessions}
        selectedId={chat.selectedId}
        open={sessionsOpen}
        collapsed={sessionsCollapsed}
        loading={chat.loading}
        onClose={() => setSessionsOpen(false)}
        onToggleCollapsed={() => setSessionsCollapsed((collapsed) => !collapsed)}
        onCreate={() => setCreationDialogOpen(true)}
        onSelect={chat.selectSession}
        onDelete={(sid) => void chat.deleteSession(sid)}
        onRename={(sid, name) => void chat.renameSession(sid, name)}
      />

      <section className="chat-column">
        <ChatHeader
          session={selectedSession}
          phase={chat.state.phase}
          usage={chat.state.usage}
          cost={chat.cost}
          meta={chat.meta}
          connection={chat.connection}
          theme={theme}
          onMenu={() => setSessionsOpen(true)}
          onMemory={() => setMemoryOpen((open) => !open)}
          workflowMode={selectedSession?.workflow_mode ?? "quick"}
          onTheme={toggleTheme}
          onReconnect={chat.reconnect}
          user={auth.user}
          onLogout={() => void auth.logout()}
        />
        {chat.state.error && <div className="global-error">{chat.state.error}</div>}
        <MessageList
          messages={chat.state.messages}
          activities={chat.state.activities}
          reasoning={chat.state.reasoning}
          busy={chat.state.busy}
          hasSession={Boolean(chat.selectedId)}
          sid={chat.selectedId}
        />
        <Composer
          disabled={!chat.selectedId || chat.connection !== "connected"}
          busy={chat.state.busy}
          onSend={chat.send}
          onStop={chat.stop}
        />
      </section>

      <MemoryPanel notes={chat.notes} open={memoryOpen} onClose={() => setMemoryOpen(false)} />
      <SessionCreationDialog
        open={creationDialogOpen}
        submitting={creatingSession}
        onCancel={closeCreationDialog}
        onConfirm={(mode: WorkflowMode) => {
          setCreatingSession(true);
          void chat.createSession(mode).then((created) => {
            if (created) setCreationDialogOpen(false);
          }).finally(() => {
            setCreatingSession(false);
          });
        }}
      />
      {(sessionsOpen || memoryOpen) && (
        <button
          className="mobile-backdrop mobile-only"
          aria-label="关闭侧栏"
          onClick={() => {
            setSessionsOpen(false);
            setMemoryOpen(false);
          }}
        />
      )}
    </main>
  );
}

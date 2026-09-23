import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";

import { ChatSocket } from "../../api/chatSocket";
import { metaApi } from "../../api/meta";
import { sessionsApi } from "../../api/sessions";
import type {
  AgentEvent,
  AppMeta,
  ServerMessage,
  SessionSummary,
  WorkflowMode,
} from "../../api/types";
import { chatReducer, initialChatState } from "./model";

type ConnectionState = "disconnected" | "connecting" | "connected";

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : "发生未知错误";
}

export function useChatController() {
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [notes, setNotes] = useState<string | null>(null);
  const [meta, setMeta] = useState<AppMeta | null>(null);
  const [connection, setConnection] = useState<ConnectionState>("disconnected");
  const [loading, setLoading] = useState(true);
  const [connectionAttempt, setConnectionAttempt] = useState(0);
  const [state, dispatch] = useReducer(chatReducer, initialChatState);
  const socketRef = useRef<ChatSocket | null>(null);
  const pendingToolRef = useRef<string | null>(null);

  const refreshSessions = useCallback(async () => {
    const list = await sessionsApi.list();
    setSessions(list);
    return list;
  }, []);

  const refreshNotes = useCallback(async (sid: string) => {
    const result = await sessionsApi.notes(sid);
    setNotes(result.content);
  }, []);

  useEffect(() => {
    let active = true;
    void Promise.allSettled([metaApi.get(), refreshSessions()]).then((results) => {
      if (!active) return;
      const [metaResult, sessionsResult] = results;
      if (metaResult.status === "fulfilled") setMeta(metaResult.value);
      if (sessionsResult.status === "fulfilled") {
        setSelectedId((current) => current ?? sessionsResult.value[0]?.id ?? null);
      } else {
        dispatch({ type: "transport-error", message: errorMessage(sessionsResult.reason) });
      }
      setLoading(false);
    });
    return () => {
      active = false;
    };
  }, [refreshSessions]);

  const recoverHistory = useCallback(
    async (sid: string) => {
      try {
        const detail = await sessionsApi.detail(sid);
        dispatch({ type: "history", detail });
        await refreshNotes(sid);
      } catch (cause) {
        dispatch({ type: "transport-error", message: errorMessage(cause) });
      }
    },
    [refreshNotes],
  );

  useEffect(() => {
    dispatch({ type: "reset" });
    setNotes(null);
    pendingToolRef.current = null;
    if (!selectedId) {
      setConnection("disconnected");
      return;
    }

    let active = true;
    const socket = new ChatSocket();
    socketRef.current = socket;
    setConnection("connecting");

    const handleMessage = (message: ServerMessage) => {
      if (!active) return;
      if (message.type === "history") {
        dispatch({ type: "history", detail: message.data });
        void refreshNotes(selectedId).catch((cause) =>
          dispatch({ type: "transport-error", message: errorMessage(cause) }),
        );
        return;
      }
      if (message.type === "error") {
        dispatch({ type: "transport-error", message: message.message });
        return;
      }
      if (message.type === "lagged") {
        dispatch({
          type: "transport-error",
          message: `连接较慢，丢失了 ${message.dropped} 条增量，已重新同步历史。`,
        });
        void recoverHistory(selectedId);
        return;
      }

      const event: AgentEvent = message.event;
      if ("ToolCall" in event) pendingToolRef.current = event.ToolCall.name;
      if ("ToolResult" in event) {
        if (pendingToolRef.current === "update_notes") {
          void refreshNotes(selectedId).catch((cause) =>
            dispatch({ type: "transport-error", message: errorMessage(cause) }),
          );
        }
        pendingToolRef.current = null;
      }
      if ("Done" in event || "Error" in event) {
        void refreshSessions().catch(() => undefined);
      }
      dispatch({ type: "event", event });
    };

    socket.connect(selectedId, {
      onOpen: () => active && setConnection("connected"),
      onClose: () => active && setConnection("disconnected"),
      onMessage: handleMessage,
      onProtocolError: (error) =>
        dispatch({ type: "transport-error", message: error.message }),
    });

    return () => {
      active = false;
      socket.disconnect();
      if (socketRef.current === socket) socketRef.current = null;
    };
  }, [connectionAttempt, recoverHistory, refreshNotes, refreshSessions, selectedId]);

  const createSession = useCallback(async (workflowMode: WorkflowMode): Promise<boolean> => {
    try {
      const detail = await sessionsApi.create(workflowMode);
      await refreshSessions();
      setSelectedId(detail.id);
      return true;
    } catch (cause) {
      dispatch({ type: "transport-error", message: errorMessage(cause) });
      return false;
    }
  }, [refreshSessions]);

  const deleteSession = useCallback(
    async (sid: string) => {
      try {
        if (selectedId === sid) {
          socketRef.current?.disconnect();
          socketRef.current = null;
        }
        await sessionsApi.remove(sid);
        const list = await refreshSessions();
        if (selectedId === sid) setSelectedId(list[0]?.id ?? null);
      } catch (cause) {
        dispatch({ type: "transport-error", message: errorMessage(cause) });
      }
    },
    [refreshSessions, selectedId],
  );

  const renameSession = useCallback(
    async (sid: string, name: string) => {
      try {
        await sessionsApi.rename(sid, name);
        await refreshSessions();
      } catch (cause) {
        dispatch({ type: "transport-error", message: errorMessage(cause) });
      }
    },
    [refreshSessions],
  );

  const send = useCallback(
    (text: string) => {
      const trimmed = text.trim();
      if (!trimmed || state.busy) return false;
      try {
        socketRef.current?.chat(trimmed);
        if (!socketRef.current) throw new Error("尚未连接会话");
        dispatch({ type: "user-sent", text: trimmed });
        return true;
      } catch (cause) {
        dispatch({ type: "transport-error", message: errorMessage(cause) });
        return false;
      }
    },
    [state.busy],
  );

  const stop = useCallback(() => {
    try {
      socketRef.current?.stop();
    } catch (cause) {
      dispatch({ type: "transport-error", message: errorMessage(cause) });
    }
  }, []);

  const cost = useMemo(() => {
    if (!meta) return null;
    return (
      (state.usage.prompt_tokens * meta.input_per_1m +
        state.usage.completion_tokens * meta.output_per_1m) /
      1_000_000
    );
  }, [meta, state.usage]);

  return {
    sessions,
    selectedId,
    selectSession: setSelectedId,
    notes,
    meta,
    connection,
    loading,
    state,
    cost,
    createSession,
    deleteSession,
    renameSession,
    send,
    stop,
    reconnect: () => setConnectionAttempt((attempt) => attempt + 1),
  };
}

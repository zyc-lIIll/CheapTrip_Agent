import type { ServerMessage } from "./types";
import { webSocketUrl } from "../lib/url";

export interface ChatSocketHandlers {
  onMessage(message: ServerMessage): void;
  onOpen?(): void;
  onClose?(): void;
  onProtocolError?(error: Error): void;
}

export class ChatSocket {
  private socket: WebSocket | null = null;

  connect(sid: string, handlers: ChatSocketHandlers): void {
    this.disconnect();
    const socket = new WebSocket(webSocketUrl(`/ws/${sid}`));
    this.socket = socket;
    socket.onopen = () => handlers.onOpen?.();
    socket.onclose = () => handlers.onClose?.();
    socket.onmessage = (event) => {
      try {
        handlers.onMessage(JSON.parse(String(event.data)) as ServerMessage);
      } catch (cause) {
        handlers.onProtocolError?.(
          cause instanceof Error ? cause : new Error("无法解析 WebSocket 消息"),
        );
      }
    };
  }

  chat(text: string): void {
    this.send({ type: "chat", text });
  }

  stop(): void {
    this.send({ type: "stop" });
  }

  disconnect(): void {
    this.socket?.close();
    this.socket = null;
  }

  private send(message: { type: "chat"; text: string } | { type: "stop" }): void {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
      throw new Error("WebSocket 尚未连接");
    }
    this.socket.send(JSON.stringify(message));
  }
}

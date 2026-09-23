import { lazy, Suspense, useLayoutEffect, useRef } from "react";

import type { ToolActivity, ViewMessage } from "../features/chat/model";

const Markdown = lazy(() => import("./Markdown").then((module) => ({ default: module.Markdown })));

interface Props {
  messages: ViewMessage[];
  activities: ToolActivity[];
  reasoning: string;
  busy: boolean;
  hasSession: boolean;
  sid: string | null;
}

export function MessageList({ messages, activities, reasoning, busy, hasSession, sid }: Props) {
  const listRef = useRef<HTMLDivElement>(null);
  const followOutputRef = useRef(true);
  const previousMessageCountRef = useRef(0);

  useLayoutEffect(() => {
    const previousCount = previousMessageCountRef.current;
    const lastMessage = messages.at(-1);
    if (messages.length < previousCount || (messages.length > previousCount && lastMessage?.role === "user")) {
      followOutputRef.current = true;
    }
    previousMessageCountRef.current = messages.length;

    const list = listRef.current;
    if (list && followOutputRef.current) {
      list.scrollTo({ top: list.scrollHeight, behavior: busy ? "auto" : "smooth" });
    }
  }, [activities, busy, messages, reasoning]);

  if (!hasSession) {
    return (
      <div className="welcome-state">
        <span className="welcome-kicker">YOUR NEXT STORY</span>
        <h2>下一段好时光，想在哪里发生？</h2>
        <p>新建一段旅程，我会陪你从一个模糊念头，慢慢走到可出发的完整攻略。</p>
      </div>
    );
  }

  if (messages.length === 0 && !busy) {
    return (
      <div className="welcome-state compact">
        <span className="welcome-kicker">READY TO GO</span>
        <h2>这段旅程，从哪里聊起？</h2>
        <p>目的地、同行的人，或只是一种想要的感觉，都可以成为第一句话。</p>
      </div>
    );
  }

  const artifactPaths = new Set(messages.flatMap((message) => message.artifacts ?? []));

  return (
    <div
      className="message-list"
      aria-live="polite"
      ref={listRef}
      onScroll={(event) => {
        const list = event.currentTarget;
        followOutputRef.current = list.scrollHeight - list.scrollTop - list.clientHeight < 80;
      }}
    >
      {messages.map((message) => (
        <article className={`message-row ${message.role}`} key={message.id}>
          {message.role === "assistant" && <div className="avatar">拾</div>}
          <div className={`message-card ${message.error ? "is-error" : ""}`}>
            <Suspense fallback={<p>{message.content}</p>}>
              {message.content && (
                <Markdown sid={sid ?? undefined} excludedPaths={artifactPaths}>
                  {message.content}
                </Markdown>
              )}
              {message.artifacts?.map((path) => (
                <Markdown key={path} sid={sid ?? undefined}>{`![旅行产物](${path})`}</Markdown>
              ))}
            </Suspense>
          </div>
        </article>
      ))}

      {reasoning && (
        <details className="reasoning-card">
          <summary>正在梳理思路</summary>
          <p>{reasoning}</p>
        </details>
      )}

      {activities.map((activity) => (
        <div className="tool-activity" key={activity.id}>
          <span className={activity.status === "running" ? "pulse" : "tool-done"} />
          {activity.status === "running" ? "正在查证" : "查证完成"} · {activity.name}
        </div>
      ))}

      {busy && messages.length > 0 && (
        <div className="thinking" aria-label="正在生成">
          <span />
          <span />
          <span />
        </div>
      )}
    </div>
  );
}

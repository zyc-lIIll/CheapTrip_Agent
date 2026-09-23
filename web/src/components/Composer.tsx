import { useLayoutEffect, useRef, useState } from "react";

import { Icon } from "./Icon";

interface Props {
  disabled: boolean;
  busy: boolean;
  onSend(text: string): boolean;
  onStop(): void;
}

export function Composer({ disabled, busy, onSend, onStop }: Props) {
  const [text, setText] = useState("");
  const inputRef = useRef<HTMLTextAreaElement>(null);

  useLayoutEffect(() => {
    const input = inputRef.current;
    if (!input) return;
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 160)}px`;
  }, [text]);

  const submit = () => {
    if (onSend(text)) setText("");
  };

  return (
    <div className="composer-wrap">
      <div className="composer">
        <textarea
          ref={inputRef}
          value={text}
          onChange={(event) => setText(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              submit();
            }
          }}
          disabled={disabled}
          rows={1}
          placeholder={disabled ? "先开启或连接一段旅程" : "说说你的旅行念头…"}
          aria-label="输入消息"
        />
        {busy ? (
          <button className="composer-action stop" onClick={onStop} aria-label="停止生成">
            <Icon name="stop" />
          </button>
        ) : (
          <button
            className="composer-action"
            onClick={submit}
            disabled={disabled || !text.trim()}
            aria-label="发送"
          >
            <Icon name="send" />
          </button>
        )}
      </div>
      <small>Enter 发送 · Shift + Enter 换行</small>
    </div>
  );
}

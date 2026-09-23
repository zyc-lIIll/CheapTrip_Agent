import { useState } from "react";
import ReactMarkdown, { defaultUrlTransform } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import bash from "highlight.js/lib/languages/bash";
import css from "highlight.js/lib/languages/css";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import markdown from "highlight.js/lib/languages/markdown";
import python from "highlight.js/lib/languages/python";
import rust from "highlight.js/lib/languages/rust";
import sql from "highlight.js/lib/languages/sql";
import typescript from "highlight.js/lib/languages/typescript";
import xml from "highlight.js/lib/languages/xml";

import { apiUrl, withDemoToken } from "../lib/url";
import { parseMediaPath, replaceMediaPaths } from "../features/chat/media";

const HIGHLIGHT_LANGUAGES = {
  bash,
  css,
  javascript,
  json,
  markdown,
  python,
  rust,
  sql,
  typescript,
  xml,
};

function MediaImage({ src, alt, props }: { src: string; alt?: string; props: Record<string, unknown> }) {
  const [failed, setFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  if (failed) {
    return (
      <div className="media-fallback">
        旅行图片暂时无法加载 ·{" "}
        <button
          type="button"
          onClick={() => {
            setFailed(false);
            setAttempt((value) => value + 1);
          }}
        >
          重新加载
        </button>
        <a href={src} target="_blank" rel="noreferrer">
          新窗口打开
        </a>
      </div>
    );
  }
  return (
    <a href={src} target="_blank" rel="noreferrer" className="media-link">
      <img
        {...props}
        key={attempt}
        src={src}
        alt={alt || "旅行图片"}
        loading="lazy"
        onError={() => setFailed(true)}
      />
    </a>
  );
}

function localMediaUrl(path: string, currentSid?: string): string | null {
  if (!currentSid) return null;
  const parsed = parseMediaPath(path, currentSid);
  if (!parsed) return null;
  const encodedPath = parsed.relativePath
    .split("/")
    .map((segment) => encodeURIComponent(segment))
    .join("/");
  return withDemoToken(apiUrl(`/media/${parsed.kind}/${parsed.sid}/${encodedPath}`)).toString();
}

function urlTransform(
  url: string,
  sid?: string,
  excludedPaths: ReadonlySet<string> = new Set<string>(),
): string {
  const mediaUrl = localMediaUrl(url, sid);
  const normalized = sid ? parseMediaPath(url, sid)?.path : null;
  if (normalized && excludedPaths.has(normalized)) return "";
  return mediaUrl ?? defaultUrlTransform(url);
}

export function Markdown({
  children,
  sid,
  excludedPaths,
}: {
  children: string;
  sid?: string;
  excludedPaths?: ReadonlySet<string>;
}) {
  return (
    <div className="markdown-body">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[[rehypeHighlight, { languages: HIGHLIGHT_LANGUAGES }]]}
        urlTransform={(url) => urlTransform(url, sid, excludedPaths)}
        components={{
          a: ({ children: linkChildren, ...props }) => (
            <a {...props} target="_blank" rel="noreferrer">
              {linkChildren}
            </a>
          ),
          img: ({ alt, src, ...props }) => {
            if (!src) return null;
            return <MediaImage src={src} alt={alt} props={props} />;
          },
        }}
      >
        {replaceMediaPaths(children, sid, excludedPaths)}
      </ReactMarkdown>
    </div>
  );
}

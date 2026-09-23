import type { SessionDetail } from "../../api/types.js";

import { extractMediaPaths, replaceMediaPaths } from "./media.js";
import { chatReducer, initialChatState } from "./model.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function equal<T>(actual: T, expected: T, message: string): void {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`${message}: ${JSON.stringify(actual)} !== ${JSON.stringify(expected)}`);
  }
}

const sid = "trip_1";
const usage = { prompt_tokens: 1, completion_tokens: 2, total_tokens: 3 };

function detail(messages: SessionDetail["messages"]): SessionDetail {
  return {
    id: sid,
    name: null,
    phase: 2,
    workflow_mode: "full",
    quick_mode: "auto",
    usage,
    messages,
    updated_at: null,
  };
}

function testParser(): void {
  equal(
    extractMediaPaths(
      "maps/trip_1/overview_1.PNG hotels/trip_1/hotel_1/review.v2-1.jpeg maps/trip_1/overview_1.png",
      sid,
    ),
    ["maps/trip_1/overview_1.png", "hotels/trip_1/hotel_1/review.v2-1.jpeg"],
    "maps/hotels paths normalize and dedupe",
  );

  const rejected = [
    "maps/other/overview.png",
    "/maps/trip_1/overview.png",
    "https://example.test/maps/trip_1/overview.png",
    "maps/trip_1/../overview.png",
    "maps/trip_1/overview.png/../other.png",
    "maps/trip_1/overview.png\\other.png",
    "maps/trip_1/overview.png?download=1",
    "maps/trip_1/overview.txt",
    "mapss/trip_1/overview.png",
    "maps//overview.png",
    "maps/trip_1/.hidden.png",
  ];
  for (const value of rejected) {
    assert(extractMediaPaths(value, sid).length === 0, `拒绝非法媒体路径: ${value}`);
  }
}

function testAssistantRepeat(): void {
  const rendered = replaceMediaPaths(
    "说明 maps/trip_1/overview.png；再次提到 maps/trip_1/overview.png，仍保留这段说明。",
    sid,
  );
  equal((rendered.match(/!\[旅行产物\]/g) ?? []).length, 1, "assistant path renders once");
  assert(rendered.includes("再次提到") && rendered.includes("仍保留"), "assistant text remains");

  const excluded = replaceMediaPaths(
    "说明 ![地图](maps/trip_1/overview.png) 后仍保留。",
    sid,
    new Set(["maps/trip_1/overview.png"]),
  );
  equal(excluded, "说明 地图 后仍保留。", "excluded Markdown media becomes alt text");
  equal(
    replaceMediaPaths("![外链](https://example.test/map.png)", sid),
    "![外链](https://example.test/map.png)",
    "external Markdown images remain unchanged",
  );
  equal(
    replaceMediaPaths("![未排除](maps/trip_1/overview.png)", sid),
    "![未排除](maps/trip_1/overview.png)",
    "unexcluded Markdown media remains renderable",
  );
}

function testHistoryAndRealtime(): void {
  const state = chatReducer(
    initialChatState,
    {
      type: "history",
      detail: detail([
        { role: "user", content: "去哪里" },
        { role: "tool", content: "普通工具文本 maps/trip_1/overview.png" },
        { role: "tool", content: "重复 maps/trip_1/overview.png 与 hotels/trip_1/hotel_1/photo.jpg" },
        { role: "assistant", content: "我已整理 maps/trip_1/overview.png 的说明" },
      ]),
    },
  );
  equal(
    state.messages.map(({ role, content, artifacts }) => ({ role, content, artifacts })),
    [
      { role: "user", content: "去哪里", artifacts: undefined },
      { role: "assistant", content: "", artifacts: ["maps/trip_1/overview.png"] },
      {
        role: "assistant",
        content: "",
        artifacts: ["hotels/trip_1/hotel_1/photo.jpg"],
      },
      {
        role: "assistant",
        content: "我已整理 maps/trip_1/overview.png 的说明",
        artifacts: undefined,
      },
    ],
    "history restores tool artifacts in order and preserves正文",
  );

  let realtime = state;
  realtime = chatReducer(realtime, {
    type: "event",
    event: { ToolCall: { name: "map", args: "{}" } },
  });
  realtime = chatReducer(realtime, {
    type: "event",
    event: { ToolResult: "maps/trip_1/new.png" },
  });
  assert(realtime.activities.at(-1)?.status === "done", "tool result completes activity");
  realtime = chatReducer(realtime, {
    type: "event",
    event: { ToolCall: { name: "hotel", args: "{}" } },
  });
  realtime = chatReducer(realtime, {
    type: "event",
    event: { ToolResult: "hotel found, no image" },
  });
  realtime = chatReducer(realtime, {
    type: "event",
    event: { ToolResult: "maps/trip_1/new.png" },
  });
  equal(
    realtime.messages.flatMap((message) => message.artifacts ?? []),
    ["maps/trip_1/overview.png", "hotels/trip_1/hotel_1/photo.jpg", "maps/trip_1/new.png"],
    "realtime artifacts are ordered, ordinary text is ignored, and duplicates are removed",
  );
}

testParser();
testAssistantRepeat();
testHistoryAndRealtime();
console.log("chat media/model tests passed");

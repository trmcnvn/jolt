import { describe, expect, it } from "vitest";
import type { SessionMessageEntry } from "./session-doc/messages";
import {
  projectTranscript,
  refreshTranscriptLivePage,
  transcriptBootstrap
} from "./session-doc/transcript-page";

const message = (index: number): SessionMessageEntry => ({
  id: `m${index}`,
  role: index % 2 === 0 ? "user" : "assistant",
  parts: [{ id: "t0", kind: "text", text: `message ${index}` }],
  createdAt: index,
  deviceId: "d",
  status: "complete"
});

describe("transcript projection", () => {
  it("builds stable pages and a tail covering at least 64 messages", () => {
    const projection = projectTranscript(Array.from({ length: 90 }, (_, index) => message(index)));
    expect(projection.pages).toHaveLength(3);
    expect(projection.manifest.totalMessages).toBe(90);
    expect(projection.manifest.turns).toHaveLength(45);
    const bootstrap = transcriptBootstrap(projection);
    expect(bootstrap.pages.flatMap((page) => page.messages).length).toBeGreaterThanOrEqual(64);
    expect(bootstrap.pages.at(-1)?.messages.at(-1)?.id).toBe("m89");
  });

  it("refreshes only the mutable live page", () => {
    const projection = projectTranscript(Array.from({ length: 40 }, (_, index) => message(index)));
    const live = projection.pages.at(-1)!;
    const changed = live.messages.map((entry, index) =>
      index + 1 === live.messages.length
        ? { ...entry, parts: [{ id: "t0", kind: "text" as const, text: "streamed" }] }
        : entry
    );
    const refreshed = refreshTranscriptLivePage(projection, changed);
    expect(refreshed.pages[0]).toEqual(projection.pages[0]);
    expect(refreshed.pages.at(-1)?.revision).not.toBe(live.revision);
    expect(refreshed.manifest.catalogRevision).toBe(projection.manifest.catalogRevision);
  });

  it("keeps continuations joined with their root", () => {
    const root = message(0);
    const continuation: SessionMessageEntry = {
      ...message(1),
      id: "m0-c1",
      continuationOf: root.id
    };
    const projection = projectTranscript([root, continuation]);
    expect(projection.pages[0]?.messages).toHaveLength(1);
    expect(projection.pages[0]?.messages[0]?.parts).toHaveLength(2);
  });
});

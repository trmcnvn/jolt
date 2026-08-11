import { describe, expect, it } from "vitest";
import { parseDiffSidecar } from "./diff-sidecar";

const sidecar = {
  chatId: "chat-1",
  deviceId: "device-1",
  checkoutPath: "/repo",
  manifest: {
    catalogRevision: "catalog",
    checkoutId: "checkout",
    deviceId: "device-1",
    cwd: "/repo",
    vcs: "git",
    files: [{
      id: "file",
      path: "a.rs",
      status: "modified",
      additions: 1,
      deletions: 1,
      binary: false,
      rowCount: 3,
      estimatedBytes: 100,
      completeness: "complete",
      pageIds: ["page"]
    }],
    pages: [{
      id: "page",
      fileId: "file",
      firstRow: 0,
      rowCount: 3,
      noticeCount: 0,
      hunkCount: 1,
      lineCount: 2,
      estimatedBytes: 100
    }],
    additions: 1,
    deletions: 1,
    truncated: false,
    updatedAt: "2026-08-06T00:00:00Z"
  },
  pages: [{ id: "page", catalogRevision: "catalog", fileId: "file", patch: "diff --git a/a.rs b/a.rs\n" }],
  publishedAt: 1
};

describe("parseDiffSidecar", () => {
  it("accepts the paged projection wire shape", () => {
    const parsed = parseDiffSidecar(sidecar);
    expect(parsed?.manifest.files[0]?.path).toBe("a.rs");
    expect(parsed?.manifest.pages[0]?.splitLineCount).toBe(2);
  });

  it("preserves split-row estimates", () => {
    const manifest = {
      ...sidecar.manifest,
      pages: [{ ...sidecar.manifest.pages[0], splitLineCount: 1 }]
    };
    expect(parseDiffSidecar({ ...sidecar, manifest })?.manifest.pages[0]?.splitLineCount).toBe(1);
  });

  it("rejects malformed nested descriptors", () => {
    expect(parseDiffSidecar({ ...sidecar, manifest: { ...sidecar.manifest, files: [{ id: 1 }] } })).toBeUndefined();
  });
});

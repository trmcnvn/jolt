/** Non-CRDT sidecar payloads served by SessionRoom. */
import type { SessionMessageEntry } from "./messages";

export interface SessionTail {
  readonly chatId: string;
  readonly schemaVersion: number;
  readonly messages: ReadonlyArray<SessionMessageEntry>;
  readonly totalMessages: number;
  readonly updatedAt: number;
}

export type DiffCompleteness = "complete" | "binary" | "snapshotTruncated" | "oversizedLine";

export interface DiffPageDescriptor {
  readonly id: string;
  readonly fileId: string;
  readonly firstRow: number;
  readonly rowCount: number;
  readonly noticeCount: number;
  readonly hunkCount: number;
  readonly lineCount: number;
  readonly estimatedBytes: number;
}

export interface DiffFileDescriptor {
  readonly id: string;
  readonly path: string;
  readonly oldPath?: string;
  readonly status: string;
  readonly additions: number;
  readonly deletions: number;
  readonly binary: boolean;
  readonly rowCount: number;
  readonly estimatedBytes: number;
  readonly completeness: DiffCompleteness;
  readonly pageIds: ReadonlyArray<string>;
}

export interface CheckoutDiffManifest {
  readonly catalogRevision: string;
  readonly checkoutId: string;
  readonly deviceId: string;
  readonly cwd: string;
  readonly vcs: "git" | "jujutsu";
  readonly label?: string;
  readonly files: ReadonlyArray<DiffFileDescriptor>;
  readonly pages: ReadonlyArray<DiffPageDescriptor>;
  readonly additions: number;
  readonly deletions: number;
  readonly truncated: boolean;
  readonly updatedAt: string;
}

export interface CheckoutDiffPage {
  readonly id: string;
  readonly catalogRevision: string;
  readonly fileId: string;
  readonly patch: string;
}

export interface DiffSidecar {
  readonly chatId: string;
  readonly deviceId: string;
  readonly checkoutPath: string;
  readonly branch?: string;
  readonly headSha?: string;
  readonly manifest: CheckoutDiffManifest;
  readonly pages: ReadonlyArray<CheckoutDiffPage>;
  readonly publishedAt: number;
}

export interface StoredDiffSidecar extends Omit<DiffSidecar, "pages"> {
  readonly pages: readonly [];
}

const objectValue = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null ? Object.fromEntries(Object.entries(value)) : undefined;

const parsePageDescriptor = (value: unknown): DiffPageDescriptor | undefined => {
  const input = objectValue(value);
  if (!input || typeof input.id !== "string" || typeof input.fileId !== "string") return undefined;
  if (typeof input.firstRow !== "number" || typeof input.rowCount !== "number" || typeof input.estimatedBytes !== "number") return undefined;
  if (typeof input.noticeCount !== "number" || typeof input.hunkCount !== "number" || typeof input.lineCount !== "number") return undefined;
  return {
    id: input.id,
    fileId: input.fileId,
    firstRow: input.firstRow,
    rowCount: input.rowCount,
    noticeCount: input.noticeCount,
    hunkCount: input.hunkCount,
    lineCount: input.lineCount,
    estimatedBytes: input.estimatedBytes
  };
};

const parseFileDescriptor = (value: unknown): DiffFileDescriptor | undefined => {
  const input = objectValue(value);
  if (!input || typeof input.id !== "string" || typeof input.path !== "string" || typeof input.status !== "string") return undefined;
  if (typeof input.additions !== "number" || typeof input.deletions !== "number" || typeof input.binary !== "boolean") return undefined;
  if (typeof input.rowCount !== "number" || typeof input.estimatedBytes !== "number") return undefined;
  if (input.completeness !== "complete" && input.completeness !== "binary" && input.completeness !== "snapshotTruncated" && input.completeness !== "oversizedLine") return undefined;
  if (!Array.isArray(input.pageIds) || input.pageIds.some((id) => typeof id !== "string")) return undefined;
  return {
    id: input.id,
    path: input.path,
    ...(typeof input.oldPath === "string" ? { oldPath: input.oldPath } : {}),
    status: input.status,
    additions: input.additions,
    deletions: input.deletions,
    binary: input.binary,
    rowCount: input.rowCount,
    estimatedBytes: input.estimatedBytes,
    completeness: input.completeness,
    pageIds: input.pageIds
  };
};

const parseManifest = (value: unknown): CheckoutDiffManifest | undefined => {
  const input = objectValue(value);
  if (!input || typeof input.catalogRevision !== "string" || typeof input.checkoutId !== "string") return undefined;
  if (typeof input.deviceId !== "string" || typeof input.cwd !== "string" || typeof input.updatedAt !== "string") return undefined;
  if (input.vcs !== "git" && input.vcs !== "jujutsu") return undefined;
  if (typeof input.additions !== "number" || typeof input.deletions !== "number" || typeof input.truncated !== "boolean") return undefined;
  if (!Array.isArray(input.files) || !Array.isArray(input.pages)) return undefined;
  const files = input.files.map(parseFileDescriptor);
  const pages = input.pages.map(parsePageDescriptor);
  if (files.some((file) => file === undefined) || pages.some((page) => page === undefined)) return undefined;
  return {
    catalogRevision: input.catalogRevision,
    checkoutId: input.checkoutId,
    deviceId: input.deviceId,
    cwd: input.cwd,
    vcs: input.vcs,
    ...(typeof input.label === "string" ? { label: input.label } : {}),
    files: files.flatMap((file) => file === undefined ? [] : [file]),
    pages: pages.flatMap((page) => page === undefined ? [] : [page]),
    additions: input.additions,
    deletions: input.deletions,
    truncated: input.truncated,
    updatedAt: input.updatedAt
  };
};

export const parseDiffSidecar = (value: unknown): DiffSidecar | undefined => {
  const input = objectValue(value);
  if (!input || typeof input.chatId !== "string" || typeof input.deviceId !== "string") return undefined;
  if (typeof input.checkoutPath !== "string" || typeof input.publishedAt !== "number") return undefined;
  const manifest = parseManifest(input.manifest);
  if (!manifest || !Array.isArray(input.pages)) return undefined;
  const pages: CheckoutDiffPage[] = [];
  for (const candidate of input.pages) {
    const page = objectValue(candidate);
    if (!page || typeof page.id !== "string" || typeof page.catalogRevision !== "string") return undefined;
    if (typeof page.fileId !== "string" || typeof page.patch !== "string") return undefined;
    pages.push({ id: page.id, catalogRevision: page.catalogRevision, fileId: page.fileId, patch: page.patch });
  }
  return {
    chatId: input.chatId,
    deviceId: input.deviceId,
    checkoutPath: input.checkoutPath,
    ...(typeof input.branch === "string" ? { branch: input.branch } : {}),
    ...(typeof input.headSha === "string" ? { headSha: input.headSha } : {}),
    manifest,
    pages,
    publishedAt: input.publishedAt
  };
};

export interface DiffSummary {
  readonly fileCount: number;
  readonly additions: number;
  readonly deletions: number;
  readonly publishedAt: number;
}

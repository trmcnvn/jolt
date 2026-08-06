/**
 * Tail materialization from the document's plain JSON shape. The Durable
 * Object does not need a Loro Mirror.
 */
import { LoroDoc } from "loro-crdt";
import { SESSION_SCHEMA_VERSION, TAIL_MESSAGE_COUNT } from "./constants";
import { joinContinuations, type SessionMessageEntry } from "./messages";
import type { SessionTail } from "./sidecar";

/** Read the doc's message entries without a Mirror (used by the DO for tail
 * materialization). `doc.toJSON()` yields the plain state shape for
 * lists-of-maps. */
export const readMessageEntries = (doc: LoroDoc): ReadonlyArray<SessionMessageEntry> => {
  const json = doc.toJSON() as { messages?: SessionMessageEntry[] };
  return json.messages ?? [];
};

/** Materialize only a physical message-list range. This is the streaming hot
 * path: tool/text updates refresh the mutable page without converting the
 * complete document or command ledger to JSON. */
export const readMessageEntryRange = (
  doc: LoroDoc,
  start: number,
  end: number
): ReadonlyArray<SessionMessageEntry> => {
  const list = doc.getList("messages");
  const entries: SessionMessageEntry[] = [];
  for (let index = start; index < Math.min(end, list.length); index++) {
    const value = list.get(index);
    if (typeof value !== "object" || value === null || !("toJSON" in value)) continue;
    const toJSON = value.toJSON;
    if (typeof toJSON !== "function") continue;
    const entry: unknown = toJSON.call(value);
    if (
      typeof entry !== "object" || entry === null ||
      !("id" in entry) || typeof entry.id !== "string" ||
      !("role" in entry) || !["user", "assistant", "system"].includes(String(entry.role)) ||
      !("parts" in entry) || !Array.isArray(entry.parts) ||
      !("createdAt" in entry) || typeof entry.createdAt !== "number" ||
      !("deviceId" in entry) || typeof entry.deviceId !== "string"
    ) continue;
    // Core scalar/container shape is validated above. Part payloads are the
    // canonical SessionRoom schema and remain forward-compatible at render.
    entries.push(entry as SessionMessageEntry);
  }
  return entries;
};

/** Materialize the DO's `tail` slot (§5 L2): last-N messages with
 * continuations joined, plus enough meta for the client to render instantly
 * and know how much history the full sync will bring. */
export const materializeTail = (
  doc: LoroDoc,
  now: number,
  tailCount: number = TAIL_MESSAGE_COUNT
): SessionTail => {
  const json = doc.toJSON() as {
    meta?: { chatId?: string; schemaVersion?: number };
    messages?: SessionMessageEntry[];
  };
  const all = joinContinuations(json.messages ?? []);
  return {
    chatId: json.meta?.chatId ?? "",
    schemaVersion: json.meta?.schemaVersion ?? SESSION_SCHEMA_VERSION,
    messages: all.slice(-tailCount),
    totalMessages: all.length,
    updatedAt: now
  };
};

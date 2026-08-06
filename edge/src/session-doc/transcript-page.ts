import type { SessionMessageEntry } from "./messages";
import { joinContinuations } from "./messages";

export const TRANSCRIPT_PAGE_MESSAGE_COUNT = 32;
export const TRANSCRIPT_PAGE_TARGET_BYTES = 384 * 1024;
export const TRANSCRIPT_BOOTSTRAP_MESSAGE_COUNT = 64;

export interface TranscriptPageDescriptor {
  readonly id: string;
  readonly revision: string;
  readonly firstOrdinal: number;
  readonly messageCount: number;
  readonly estimatedBytes: number;
  readonly previousPageId?: string;
  readonly nextPageId?: string;
  readonly live: boolean;
}

export interface TranscriptTurnDescriptor {
  readonly messageId: string;
  readonly ordinal: number;
  readonly pageId: string;
  readonly promptPreview: string;
  readonly replyPreview?: string;
}

export interface TranscriptManifest {
  readonly catalogRevision: string;
  readonly totalMessages: number;
  readonly pages: ReadonlyArray<TranscriptPageDescriptor>;
  readonly turns: ReadonlyArray<TranscriptTurnDescriptor>;
}

export interface TranscriptPage {
  readonly id: string;
  readonly revision: string;
  readonly firstOrdinal: number;
  readonly messages: ReadonlyArray<SessionMessageEntry>;
}

export interface TranscriptBootstrap {
  readonly sequence: number;
  readonly manifest: TranscriptManifest;
  readonly pages: ReadonlyArray<TranscriptPage>;
}

interface MutableTurn {
  messageId: string;
  ordinal: number;
  pageId: string;
  promptPreview: string;
  replyPreview?: string;
}

const encodedBytes = (value: unknown): number => new TextEncoder().encode(JSON.stringify(value)).length;

const hash = (value: unknown): string => {
  const bytes = new TextEncoder().encode(JSON.stringify(value));
  let state = 0x811c9dc5;
  for (const byte of bytes) {
    state ^= byte;
    state = Math.imul(state, 0x01000193) >>> 0;
  }
  return state.toString(16).padStart(8, "0");
};

const preview = (entry: SessionMessageEntry, limit: number): string => {
  const flattened = entry.parts
    .flatMap((part) => part.kind === "text" ? [part.text] : [])
    .join(" ")
    .replace(/\s+/g, " ")
    .trim();
  return flattened.length <= limit ? flattened : `${flattened.slice(0, Math.max(0, limit - 1))}…`;
};

export interface TranscriptProjection {
  readonly manifest: TranscriptManifest;
  readonly pages: ReadonlyArray<TranscriptPage>;
}

export const projectTranscript = (
  rawEntries: ReadonlyArray<SessionMessageEntry>
): TranscriptProjection => {
  const messages = [...joinContinuations(rawEntries)];
  const pageMessages: SessionMessageEntry[][] = [];
  let current: SessionMessageEntry[] = [];
  let currentBytes = 0;
  for (const message of messages) {
    const bytes = encodedBytes(message);
    if (
      current.length > 0 &&
      (current.length >= TRANSCRIPT_PAGE_MESSAGE_COUNT || currentBytes + bytes > TRANSCRIPT_PAGE_TARGET_BYTES)
    ) {
      pageMessages.push(current);
      current = [];
      currentBytes = 0;
    }
    current.push(message);
    currentBytes += bytes;
  }
  if (current.length > 0) pageMessages.push(current);

  let ordinal = 0;
  const pages: TranscriptPage[] = pageMessages.map((entries) => {
    const page: TranscriptPage = {
      id: entries[0]?.id ?? `page-${ordinal}`,
      revision: hash(entries),
      firstOrdinal: ordinal,
      messages: entries
    };
    ordinal += entries.length;
    return page;
  });
  const descriptors: TranscriptPageDescriptor[] = pages.map((page, index) => ({
    id: page.id,
    revision: page.revision,
    firstOrdinal: page.firstOrdinal,
    messageCount: page.messages.length,
    estimatedBytes: encodedBytes(page.messages),
    ...(index > 0 ? { previousPageId: pages[index - 1]!.id } : {}),
    ...(index + 1 < pages.length ? { nextPageId: pages[index + 1]!.id } : {}),
    live: index + 1 === pages.length
  }));

  const turns: MutableTurn[] = [];
  for (const page of pages) {
    for (const [offset, message] of page.messages.entries()) {
      if (message.role === "user") {
        turns.push({
          messageId: message.id,
          ordinal: page.firstOrdinal + offset,
          pageId: page.id,
          promptPreview: preview(message, 160)
        });
      } else if (message.role === "assistant") {
        const turn = turns.at(-1);
        if (turn && turn.replyPreview === undefined) {
          const text = preview(message, 200);
          if (text.length > 0) turn.replyPreview = text;
        }
      }
    }
  }
  return {
    manifest: {
      catalogRevision: hash(descriptors.map(({ id, firstOrdinal, messageCount }) => ({ id, firstOrdinal, messageCount }))),
      totalMessages: messages.length,
      pages: descriptors,
      turns
    },
    pages
  };
};

export const refreshTranscriptLivePage = (
  projection: TranscriptProjection,
  rawEntries: ReadonlyArray<SessionMessageEntry>
): TranscriptProjection => {
  const descriptor = projection.manifest.pages.at(-1);
  const previous = projection.pages.at(-1);
  if (!descriptor || !previous) return projection;
  const messages = [...joinContinuations(rawEntries)];
  const page: TranscriptPage = {
    id: previous.id,
    revision: hash(messages),
    firstOrdinal: previous.firstOrdinal,
    messages
  };
  const updatedDescriptor: TranscriptPageDescriptor = {
    ...descriptor,
    revision: page.revision,
    messageCount: messages.length,
    estimatedBytes: encodedBytes(messages)
  };
  return {
    manifest: {
      ...projection.manifest,
      pages: [...projection.manifest.pages.slice(0, -1), updatedDescriptor]
    },
    pages: [...projection.pages.slice(0, -1), page]
  };
};

export const transcriptBootstrap = (projection: TranscriptProjection): TranscriptBootstrap => {
  const tail: TranscriptPage[] = [];
  let count = 0;
  for (let index = projection.pages.length - 1; index >= 0; index--) {
    const page = projection.pages[index];
    if (!page) continue;
    tail.push(page);
    count += page.messages.length;
    if (count >= TRANSCRIPT_BOOTSTRAP_MESSAGE_COUNT) break;
  }
  tail.reverse();
  return { sequence: 0, manifest: projection.manifest, pages: tail };
};

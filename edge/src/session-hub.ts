/**
 * SessionHub — wasm-free command mailbox and transcript projection relay.
 *
 * A chat has one immutable host device. The hub stores typed command current
 * state plus a bounded log of host-authored transcript projection deltas.
 * Sealed transcript pages live in R2 and are referenced by the host-published
 * manifest; this class never imports or materializes a CRDT document.
 */
import { createBlobStore, getJsonBlob, putJsonBlob, type BlobStore } from "./blobs";
import { AUTH_USER_HEADER, type Env } from "./env";
import { parseDiffSidecar, type StoredDiffSidecar } from "./diff-sidecar";

const HOST_TAG = "host";
const VIEWER_TAG = "viewer";
const DIFF_TAG = "diff";
const DAY_MS = 24 * 60 * 60 * 1000;
const MAX_ID_BYTES = 128;
const MAX_COMMAND_BYTES = 512 * 1024;
const MAX_COMMAND_PAGE_BYTES = 2 * 1024 * 1024;
const MAX_MANIFEST_BYTES = 2 * 1024 * 1024;
const MAX_LIVE_PAGE_BYTES = 2 * 1024 * 1024;
const MAX_DELTA_BYTES = 256 * 1024;
const MAX_RESOLUTION_BYTES = 64 * 1024;
const DELTA_BASE_REQUEST_ROWS = 200;
const DELTA_BASE_REQUEST_BYTES = 512 * 1024;
const TERMINAL_STATUSES = new Set([
  "applied",
  "rejected",
  "expired",
  "superseded",
  "cancelled"
]);

interface SocketState {
  readonly userId: string;
  readonly role: "host" | "viewer" | "diff";
  readonly device: string;
  readonly lease?: number;
}

export interface SubmittedHubCommand {
  readonly id: string;
  readonly kind: string;
  readonly payload: unknown;
  readonly issuedBy: string;
  readonly issuedAt: number;
  readonly expiresAt: number;
  readonly basedOn?: unknown;
}

export interface StoredHubCommand extends SubmittedHubCommand {
  readonly seq: number;
  readonly updateRevision: number;
  readonly deliveryState: "pending" | "claimed" | "terminal";
  readonly status: "pending" | "applied" | "rejected" | "expired" | "superseded" | "cancelled";
  readonly claimedBy?: string;
  readonly claimToken?: string;
  readonly resolution?: string;
}

interface HostPublishBase {
  readonly type: "publishBase";
  readonly requestId: string;
  readonly publishId: string;
  readonly lease: number;
  readonly manifest: unknown;
  readonly livePage?: unknown;
}

interface HostPublishDelta {
  readonly type: "publishDelta";
  readonly requestId: string;
  readonly publishId: string;
  readonly lease: number;
  readonly pageId: string;
  readonly basePageRevision: string;
  readonly pageRevision: string;
  readonly frame: unknown;
}

interface HostClaimCommand {
  readonly type: "claimCommand";
  readonly requestId: string;
  readonly lease: number;
  readonly commandId: string;
}

interface HostResolveCommand {
  readonly type: "resolveCommand";
  readonly requestId: string;
  readonly lease: number;
  readonly commandId: string;
  readonly claimToken: string;
  readonly status: StoredHubCommand["status"];
  readonly resolution?: string;
}

type HostFrame = HostPublishBase | HostPublishDelta | HostClaimCommand | HostResolveCommand;

interface ProjectionDeltaRow {
  readonly sequence: number;
  readonly delta: {
    readonly pageId: string;
    readonly pageRevision: string;
    readonly frame: unknown;
  };
}

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

const validId = (value: string): boolean =>
  value.length > 0 && value.length <= MAX_ID_BYTES && /^[A-Za-z0-9_-]+$/.test(value);

const finiteNumber = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value);

const nullableString = (value: unknown): boolean => value === null || typeof value === "string";

const RUN_HARNESSES = new Set(["claude-code", "codex", "pi", "mock"]);
const REASONING_LEVELS = new Set([
  "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "ultracode", "ultrathink"
]);
const SANDBOX_LEVELS = new Set(["read-only", "workspace-write", "danger-full-access"]);

const validRunRequest = (value: unknown): boolean => {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  return "prompt" in value && typeof value.prompt === "string"
    && (!("harness" in value) || value.harness === null
      || (typeof value.harness === "string" && RUN_HARNESSES.has(value.harness)))
    && (!("model" in value) || value.model === undefined || nullableString(value.model))
    && (!("reasoning" in value) || value.reasoning === undefined || value.reasoning === null
      || (typeof value.reasoning === "string" && REASONING_LEVELS.has(value.reasoning)))
    && (!("modelOptions" in value) || value.modelOptions === undefined
      || (typeof value.modelOptions === "object" && value.modelOptions !== null
        && !Array.isArray(value.modelOptions)))
    && "cwd" in value && typeof value.cwd === "string"
    && "sandbox" in value && typeof value.sandbox === "string"
    && SANDBOX_LEVELS.has(value.sandbox)
    && (!("autoApprove" in value) || value.autoApprove === undefined
      || typeof value.autoApprove === "boolean")
    && (!("resume" in value) || value.resume === undefined || nullableString(value.resume))
    && (!("attachments" in value) || value.attachments === undefined
      || (Array.isArray(value.attachments)
        && value.attachments.every((item) => typeof item === "string")));
};

const validAnswer = (value: unknown): boolean => {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  return "questionId" in value && typeof value.questionId === "string"
    && "labels" in value && Array.isArray(value.labels)
    && value.labels.every((label) => typeof label === "string");
};

const validGoalOperation = (value: unknown): boolean => {
  if (typeof value !== "object" || value === null || Array.isArray(value)
    || !("action" in value) || typeof value.action !== "string") return false;
  const revision = "expectedRevision" in value ? value.expectedRevision : undefined;
  const validRevision = typeof revision === "number"
    && Number.isSafeInteger(revision) && revision >= 0;
  const tokenBudget = "tokenBudget" in value ? value.tokenBudget : undefined;
  const validBudget = tokenBudget === undefined || tokenBudget === null
    || (typeof tokenBudget === "number" && Number.isSafeInteger(tokenBudget) && tokenBudget >= 0);
  switch (value.action) {
    case "create":
      return "objective" in value && typeof value.objective === "string" && validBudget;
    case "edit":
      return "goalId" in value && typeof value.goalId === "string" && validId(value.goalId)
        && validRevision && "objective" in value && typeof value.objective === "string"
        && validBudget;
    case "pause":
    case "resume":
    case "clear":
      return "goalId" in value && typeof value.goalId === "string" && validId(value.goalId)
        && validRevision;
    default:
      return false;
  }
};

const validCommandPayload = (kind: string, value: unknown): boolean => {
  if (typeof value !== "object" || value === null || Array.isArray(value)
    || !("kind" in value) || typeof value.kind !== "string") return false;
  switch (value.kind) {
    case "run":
    case "queue":
      return kind === value.kind
        && "request" in value && validRunRequest(value.request)
        && "messageId" in value && typeof value.messageId === "string"
        && validId(value.messageId);
    case "hiddenPrompt":
      return kind === "run" && "request" in value && validRunRequest(value.request);
    case "resumeQueue":
    case "interrupt":
      return kind === value.kind;
    case "bash":
      return kind === "bash"
        && "command" in value && typeof value.command === "string"
        && "excludeFromContext" in value && typeof value.excludeFromContext === "boolean"
        && "cwd" in value && typeof value.cwd === "string"
        && "messageId" in value && typeof value.messageId === "string"
        && validId(value.messageId);
    case "steer":
      return kind === "steer"
        && "prompt" in value && typeof value.prompt === "string"
        && (!("messageId" in value) || value.messageId === undefined || value.messageId === null
          || (typeof value.messageId === "string" && validId(value.messageId)));
    case "respondInput":
      return kind === "respondInput"
        && "requestId" in value && typeof value.requestId === "string"
        && validId(value.requestId)
        && "answers" in value && Array.isArray(value.answers)
        && value.answers.every(validAnswer);
    case "goal":
      return kind === "goal" && "operation" in value && validGoalOperation(value.operation);
    default:
      return false;
  }
};

const validBasedOn = (value: unknown): boolean => {
  if (value === undefined || value === null) return true;
  if (typeof value !== "object" || Array.isArray(value)) return false;
  return (!("turnId" in value) || value.turnId === undefined || value.turnId === null
      || typeof value.turnId === "string")
    && (!("frontier" in value) || value.frontier === undefined || value.frontier === null
      || typeof value.frontier === "string");
};

export const parseSubmittedHubCommand = (input: unknown): SubmittedHubCommand | undefined => {
  if (typeof input !== "object" || input === null) return undefined;
  if (!("id" in input) || typeof input.id !== "string" || !validId(input.id)) return undefined;
  if (!("kind" in input) || typeof input.kind !== "string" || input.kind.length === 0) return undefined;
  if (!("payload" in input) || !validCommandPayload(input.kind, input.payload)) return undefined;
  if (!("issuedBy" in input) || typeof input.issuedBy !== "string" || !validId(input.issuedBy)) return undefined;
  if (!("issuedAt" in input) || !finiteNumber(input.issuedAt) || !Number.isSafeInteger(input.issuedAt)) return undefined;
  if (!("expiresAt" in input) || !finiteNumber(input.expiresAt) || !Number.isSafeInteger(input.expiresAt)) return undefined;
  if ("basedOn" in input && !validBasedOn(input.basedOn)) return undefined;
  const command: SubmittedHubCommand = {
    id: input.id,
    kind: input.kind,
    payload: input.payload,
    issuedBy: input.issuedBy,
    issuedAt: input.issuedAt,
    expiresAt: input.expiresAt,
    ...(!("basedOn" in input) || input.basedOn === undefined ? {} : { basedOn: input.basedOn })
  };
  return new TextEncoder().encode(JSON.stringify(command)).length <= MAX_COMMAND_BYTES
    ? command
    : undefined;
};

const jsonByteLength = (value: unknown): number =>
  new TextEncoder().encode(JSON.stringify(value)).length;

const sha256Hex = async (value: string): Promise<string> => {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
};

const canonicalJson = (value: unknown): string => {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  const fields = Object.entries(value)
    .filter(([, field]) => field !== undefined)
    .sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0);
  return `{${fields.map(([key, field]) => `${JSON.stringify(key)}:${canonicalJson(field)}`).join(",")}}`;
};

const parseHostFrame = (input: unknown): HostFrame | undefined => {
  if (typeof input !== "object" || input === null || !("type" in input)) return undefined;
  if (!("requestId" in input) || typeof input.requestId !== "string" || !validId(input.requestId)) return undefined;
  if (!("lease" in input) || !finiteNumber(input.lease) || input.lease < 1) return undefined;
  switch (input.type) {
    case "publishBase":
      if (!("publishId" in input) || typeof input.publishId !== "string" || !validId(input.publishId)) return undefined;
      if (!("manifest" in input)) return undefined;
      if (jsonByteLength(input.manifest) > MAX_MANIFEST_BYTES) return undefined;
      if ("livePage" in input && input.livePage !== undefined) {
        if (jsonByteLength(input.livePage) > MAX_LIVE_PAGE_BYTES) return undefined;
        const livePage = input.livePage;
        if (typeof livePage !== "object" || livePage === null || Array.isArray(livePage)
          || !("id" in livePage) || typeof livePage.id !== "string" || !validId(livePage.id)
          || !("revision" in livePage) || typeof livePage.revision !== "string"
          || livePage.revision.length === 0) return undefined;
      }
      return {
        type: "publishBase",
        requestId: input.requestId,
        publishId: input.publishId,
        lease: input.lease,
        manifest: input.manifest,
        ...(!("livePage" in input) || input.livePage === undefined ? {} : { livePage: input.livePage })
      };
    case "publishDelta":
      if (!("publishId" in input) || typeof input.publishId !== "string" || !validId(input.publishId)) return undefined;
      if (!("pageId" in input) || typeof input.pageId !== "string" || !validId(input.pageId)) return undefined;
      if (!("basePageRevision" in input) || typeof input.basePageRevision !== "string" || input.basePageRevision.length === 0) return undefined;
      if (!("pageRevision" in input) || typeof input.pageRevision !== "string" || input.pageRevision.length === 0) return undefined;
      if (!("frame" in input) || jsonByteLength(input.frame) > MAX_DELTA_BYTES) return undefined;
      return {
        type: "publishDelta",
        requestId: input.requestId,
        publishId: input.publishId,
        lease: input.lease,
        pageId: input.pageId,
        basePageRevision: input.basePageRevision,
        pageRevision: input.pageRevision,
        frame: input.frame
      };
    case "claimCommand":
      if (!("commandId" in input) || typeof input.commandId !== "string" || !validId(input.commandId)) return undefined;
      return {
        type: "claimCommand",
        requestId: input.requestId,
        lease: input.lease,
        commandId: input.commandId
      };
    case "resolveCommand": {
      if (!("commandId" in input) || typeof input.commandId !== "string" || !validId(input.commandId)) return undefined;
      if (!("claimToken" in input) || typeof input.claimToken !== "string" || !validId(input.claimToken)) return undefined;
      if (!("status" in input) || typeof input.status !== "string" || !TERMINAL_STATUSES.has(input.status)) return undefined;
      const resolution = "resolution" in input ? input.resolution : undefined;
      if (resolution !== undefined && typeof resolution !== "string") return undefined;
      if (typeof resolution === "string" && new TextEncoder().encode(resolution).length > MAX_RESOLUTION_BYTES) {
        return undefined;
      }
      return {
        type: "resolveCommand",
        requestId: input.requestId,
        lease: input.lease,
        commandId: input.commandId,
        claimToken: input.claimToken,
        status: input.status as StoredHubCommand["status"],
        ...(resolution === undefined ? {} : { resolution })
      };
    }
    default:
      return undefined;
  }
};

export class SessionHub implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  private readonly blobs: BlobStore;

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    this.blobs = createBlobStore(ctx.storage.sql);
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS commands (seq INTEGER PRIMARY KEY, command_id TEXT NOT NULL UNIQUE, canonical TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, issued_by TEXT NOT NULL, issued_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, based_on TEXT, update_revision INTEGER NOT NULL, delivery_state TEXT NOT NULL, status TEXT NOT NULL, claimed_by TEXT, claim_token TEXT, resolution TEXT)"
    );
    ctx.storage.sql.exec(
      "CREATE INDEX IF NOT EXISTS commands_state ON commands(delivery_state, seq)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS projection_deltas (seq INTEGER PRIMARY KEY, publish_id TEXT NOT NULL UNIQUE, page_id TEXT NOT NULL, page_revision TEXT NOT NULL, frame TEXT NOT NULL, byte_len INTEGER NOT NULL, created_at INTEGER NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS publications (publish_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, kind TEXT NOT NULL, created_at INTEGER NOT NULL)"
    );
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  private getMeta(key: string): string | undefined {
    const row = [...this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key)][0];
    return row?.value as string | undefined;
  }

  private setMeta(key: string, value: string): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      key,
      value
    );
  }

  private deleteMeta(key: string): void {
    this.ctx.storage.sql.exec("DELETE FROM meta WHERE key = ?", key);
  }

  private owner(userId: string, allowClaim: boolean): boolean {
    const owner = this.getMeta("owner");
    if (!owner && allowClaim) {
      this.setMeta("owner", userId);
      return true;
    }
    return owner === userId;
  }

  private projectionSequence(): number {
    return Number(this.getMeta("projectionSequence") ?? "0");
  }

  private commandRevision(): number {
    return Number(this.getMeta("commandRevision") ?? "0");
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return json({ error: "unauthenticated" }, 401);
    if (this.getMeta("retired") === "1") return json({ error: "retired" }, 410);
    const requestedChatId = url.searchParams.get("chatId");
    if (requestedChatId && validId(requestedChatId) && !this.getMeta("chatId")) {
      this.setMeta("chatId", requestedChatId);
    }

    if (url.pathname === "/diff/ws") {
      if (!this.owner(userId, false)) return json({ error: "not_found" }, 404);
      if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
        return json({ error: "expected_websocket" }, 426);
      }
      const sidecar = getJsonBlob<StoredDiffSidecar>(this.blobs, "diff-v2");
      if (!sidecar) return json({ error: "not_found" }, 404);
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1], [DIFF_TAG]);
      pair[1].serializeAttachment({ userId, role: "diff", device: "" } satisfies SocketState);
      this.send(pair[1], {
        type: "bootstrap",
        bootstrap: {
          sequence: Number(this.getMeta("diffSequence") ?? "0"),
          manifest: sidecar.manifest,
          pages: []
        }
      });
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    if (url.pathname === "/diff" && request.method === "GET") {
      if (!this.owner(userId, false)) return json({ error: "not_found" }, 404);
      const sidecar = getJsonBlob<StoredDiffSidecar>(this.blobs, "diff-v2");
      return sidecar === undefined ? json({ error: "not_found" }, 404) : json(sidecar);
    }

    if (url.pathname === "/diff" && request.method === "POST") {
      if (!this.owner(userId, true)) return json({ error: "forbidden" }, 403);
      const sidecar = parseDiffSidecar(await request.json().catch(() => undefined));
      const chatId = this.getMeta("chatId");
      if (!sidecar
        || !validId(sidecar.deviceId)
        || sidecar.deviceId !== sidecar.manifest.deviceId
        || (chatId !== undefined && sidecar.chatId !== chatId)) {
        return json({ error: "invalid_diff_sidecar" }, 400);
      }
      const assigned = this.getMeta("hostDevice");
      if (assigned && assigned !== sidecar.deviceId) {
        return json({ error: "wrong_host" }, 409);
      }
      if (!assigned) this.setMeta("hostDevice", sidecar.deviceId);
      const sequence = Number(this.getMeta("diffSequence") ?? "0") + 1;
      const stored: StoredDiffSidecar = { ...sidecar, pages: [] };
      putJsonBlob(this.blobs, "diff-v2", stored);
      this.setMeta("diffSequence", String(sequence));
      this.markBackupDirty();
      const frame = JSON.stringify({ type: "manifest", sequence, manifest: sidecar.manifest });
      for (const socket of this.ctx.getWebSockets(DIFF_TAG)) {
        try { socket.send(frame); } catch { try { socket.close(1011, "diff delivery failed"); } catch { /* closed */ } }
      }
      return json({ ok: true, catalogRevision: sidecar.manifest.catalogRevision });
    }

    if (url.pathname === "/ws") {
      if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
        return json({ error: "expected_websocket" }, 426);
      }
      const role = url.searchParams.get("role") === "host" ? "host" : "viewer";
      const device = url.searchParams.get("device") ?? "";
      if (role === "host" && !validId(device)) return json({ error: "invalid_device" }, 400);
      if (!this.owner(userId, role === "host")) return json({ error: "forbidden" }, 403);
      const pair = new WebSocketPair();
      if (role === "host") {
        const assigned = this.getMeta("hostDevice");
        if (assigned && assigned !== device) return json({ error: "wrong_host" }, 409);
        if (!assigned) this.setMeta("hostDevice", device);
        const lease = Number(this.getMeta("writerLease") ?? "0") + 1;
        this.setMeta("writerLease", String(lease));
        for (const stale of this.ctx.getWebSockets(HOST_TAG)) {
          try { stale.close(4409, "superseded host connection"); } catch { /* closed */ }
        }
        this.ctx.acceptWebSocket(pair[1], [HOST_TAG]);
        pair[1].serializeAttachment({ userId, role, device, lease } satisfies SocketState);
        this.send(pair[1], {
          type: "hostState",
          lease,
          projectionSequence: this.projectionSequence(),
          commandRevision: this.commandRevision(),
          commands: this.actionableCommands(device)
        });
      } else {
        this.ctx.acceptWebSocket(pair[1], [VIEWER_TAG]);
        pair[1].serializeAttachment({ userId, role, device } satisfies SocketState);
        this.sendBootstrap(pair[1]);
      }
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    if (url.pathname === "/bootstrap" && request.method === "GET") {
      if (!this.owner(userId, false)) return json({ error: "not_found" }, 404);
      return json(this.bootstrap());
    }

    if (url.pathname === "/commands" && request.method === "GET") {
      if (!this.owner(userId, false)) return json({ error: "not_found" }, 404);
      const after = Number(url.searchParams.get("after") ?? "0");
      if (!Number.isSafeInteger(after) || after < 0) {
        return json({ error: "invalid_command_cursor" }, 400);
      }
      const commands: StoredHubCommand[] = [];
      let bytes = 0;
      for (const row of this.ctx.storage.sql.exec(
        "SELECT seq, command_id, kind, payload, issued_by, issued_at, expires_at, based_on, update_revision, delivery_state, status, claimed_by, claim_token, resolution FROM commands WHERE update_revision > ? ORDER BY update_revision LIMIT 501",
        after
      )) {
        const command = this.decodeCommand(row);
        const commandBytes = new TextEncoder().encode(JSON.stringify(command)).length;
        if (commands.length > 0 && bytes + commandBytes > MAX_COMMAND_PAGE_BYTES) break;
        commands.push(command);
        bytes += commandBytes;
        if (commands.length === 500) break;
      }
      const commandRevision = this.commandRevision();
      const nextRevision = commands.at(-1)?.updateRevision ?? Math.min(after, commandRevision);
      return json({
        commands,
        nextRevision,
        hasMore: nextRevision < commandRevision,
        commandRevision
      });
    }

    if (url.pathname === "/command" && request.method === "POST") {
      if (!this.owner(userId, true)) return json({ error: "forbidden" }, 403);
      const command = parseSubmittedHubCommand(await request.json().catch(() => undefined));
      if (!command) return json({ error: "invalid_command" }, 400);
      const result = await this.submitCommand(command, userId);
      return json(result, result.conflict ? 409 : 200);
    }

    if (url.pathname === "/command/cancel" && request.method === "POST") {
      if (!this.owner(userId, false)) return json({ error: "not_found" }, 404);
      const input: unknown = await request.json().catch(() => undefined);
      if (typeof input !== "object" || input === null) return json({ error: "invalid_cancel" }, 400);
      if (!("commandId" in input) || typeof input.commandId !== "string" || !validId(input.commandId)) {
        return json({ error: "invalid_cancel" }, 400);
      }
      if (!("device" in input) || typeof input.device !== "string" || !validId(input.device)) {
        return json({ error: "invalid_cancel" }, 400);
      }
      const changed = this.cancelCommand(input.commandId, input.device);
      return changed ? json({ ok: true }) : json({ error: "not_pending_or_not_issuer" }, 409);
    }

    if (url.pathname === "/stats" && request.method === "GET") {
      if (!this.owner(userId, false)) return json({ error: "not_found" }, 404);
      return json(this.stats());
    }

    if (url.pathname === "/retire" && request.method === "POST") {
      if (!this.owner(userId, false)) return json({ error: "not_found" }, 404);
      await this.retire();
      return json({ ok: true });
    }

    return json({ error: "not_found" }, 404);
  }

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message !== "string") {
      ws.close(1003, "text frames only");
      return;
    }
    const state = ws.deserializeAttachment() as SocketState | null;
    if (state?.role !== "host") return;
    let input: unknown;
    try {
      input = JSON.parse(message) as unknown;
    } catch {
      this.send(ws, { type: "error", code: "invalid_json" });
      return;
    }
    const frame = parseHostFrame(input);
    if (!frame) {
      this.send(ws, { type: "error", code: "invalid_frame" });
      return;
    }
    if (frame.lease !== state.lease || frame.lease !== Number(this.getMeta("writerLease") ?? "0")) {
      this.send(ws, { type: "response", requestId: frame.requestId, ok: false, error: "stale_lease" });
      return;
    }
    switch (frame.type) {
      case "publishBase":
        this.publishBase(ws, frame);
        return;
      case "publishDelta":
        this.publishDelta(ws, frame);
        return;
      case "claimCommand":
        this.claimCommand(ws, state, frame);
        return;
      case "resolveCommand":
        this.resolveCommand(ws, frame);
        return;
    }
  }

  webSocketClose(): void {
    /* Every accepted mutation is synchronous. */
  }

  webSocketError(): void {
    /* Every accepted mutation is synchronous. */
  }

  private publishBase(ws: WebSocket, frame: HostPublishBase): void {
    const prior = this.publication(frame.publishId);
    if (prior !== undefined) {
      this.send(ws, { type: "response", requestId: frame.requestId, ok: true, sequence: prior, duplicate: true });
      return;
    }
    const sequence = this.projectionSequence() + 1;
    putJsonBlob(this.blobs, "projection-manifest", frame.manifest);
    if (frame.livePage === undefined) {
      this.blobs.delete("projection-live-base");
      this.deleteMeta("livePageId");
      this.deleteMeta("livePageRevision");
    } else {
      const livePage = frame.livePage as { id: string; revision: string };
      putJsonBlob(this.blobs, "projection-live-base", frame.livePage);
      this.setMeta("livePageId", livePage.id);
      this.setMeta("livePageRevision", livePage.revision);
    }
    this.ctx.storage.sql.exec("DELETE FROM projection_deltas");
    this.setMeta("projectionSequence", String(sequence));
    this.setMeta("projectionBaseSequence", String(sequence));
    this.recordPublication(frame.publishId, sequence, "base");
    this.markBackupDirty();
    this.send(ws, { type: "response", requestId: frame.requestId, ok: true, sequence, duplicate: false });
    this.broadcastViewers({ type: "bootstrap", bootstrap: this.bootstrap() });
  }

  private publishDelta(ws: WebSocket, frame: HostPublishDelta): void {
    const prior = this.publication(frame.publishId);
    if (prior !== undefined) {
      this.send(ws, { type: "response", requestId: frame.requestId, ok: true, sequence: prior, duplicate: true });
      return;
    }
    if (this.getMeta("livePageId") !== frame.pageId
      || this.getMeta("livePageRevision") !== frame.basePageRevision) {
      this.send(ws, {
        type: "response",
        requestId: frame.requestId,
        ok: true,
        sequence: this.projectionSequence(),
        duplicate: false,
        needBase: true
      });
      return;
    }
    const existingBudget = this.deltaBudget();
    if (existingBudget.rows >= DELTA_BASE_REQUEST_ROWS
      || existingBudget.bytes >= DELTA_BASE_REQUEST_BYTES) {
      this.send(ws, {
        type: "response",
        requestId: frame.requestId,
        ok: true,
        sequence: this.projectionSequence(),
        duplicate: false,
        needBase: true
      });
      return;
    }
    const sequence = this.projectionSequence() + 1;
    const encoded = JSON.stringify(frame.frame);
    this.ctx.storage.sql.exec(
      "INSERT INTO projection_deltas(seq, publish_id, page_id, page_revision, frame, byte_len, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
      sequence,
      frame.publishId,
      frame.pageId,
      frame.pageRevision,
      encoded,
      new TextEncoder().encode(encoded).length,
      Date.now()
    );
    this.setMeta("projectionSequence", String(sequence));
    this.setMeta("livePageRevision", frame.pageRevision);
    this.recordPublication(frame.publishId, sequence, "delta");
    this.markBackupDirty();
    const budget = this.deltaBudget();
    const needBase = budget.rows >= DELTA_BASE_REQUEST_ROWS || budget.bytes >= DELTA_BASE_REQUEST_BYTES;
    this.send(ws, {
      type: "response",
      requestId: frame.requestId,
      ok: true,
      sequence,
      duplicate: false,
      needBase
    });
    this.broadcastViewers({
      type: "delta",
      sequence,
      delta: {
        pageId: frame.pageId,
        pageRevision: frame.pageRevision,
        frame: frame.frame
      }
    });
  }

  private claimCommand(ws: WebSocket, state: SocketState, frame: HostClaimCommand): void {
    const command = this.loadCommand(frame.commandId);
    if (!command) {
      this.send(ws, { type: "response", requestId: frame.requestId, ok: false, error: "command_not_found" });
      return;
    }
    if (command.deliveryState === "terminal") {
      this.send(ws, { type: "response", requestId: frame.requestId, ok: false, error: "command_terminal", command });
      return;
    }
    if (command.deliveryState === "claimed") {
      if (command.claimedBy === state.device) {
        this.send(ws, {
          type: "response",
          requestId: frame.requestId,
          ok: true,
          command,
          duplicate: true
        });
      } else {
        this.send(ws, { type: "response", requestId: frame.requestId, ok: false, error: "command_claimed" });
      }
      return;
    }
    const claimToken = crypto.randomUUID();
    const revision = this.commandRevision() + 1;
    this.ctx.storage.sql.exec(
      "UPDATE commands SET delivery_state = 'claimed', claimed_by = ?, claim_token = ?, update_revision = ? WHERE command_id = ? AND delivery_state = 'pending'",
      state.device,
      claimToken,
      revision,
      frame.commandId
    );
    this.setMeta("commandRevision", String(revision));
    this.markBackupDirty();
    const claimed = this.loadCommand(frame.commandId);
    this.send(ws, { type: "response", requestId: frame.requestId, ok: true, command: claimed, duplicate: false });
    if (claimed) this.broadcastCommand(claimed);
  }

  private resolveCommand(ws: WebSocket, frame: HostResolveCommand): void {
    const command = this.loadCommand(frame.commandId);
    if (!command) {
      this.send(ws, { type: "response", requestId: frame.requestId, ok: false, error: "command_not_found" });
      return;
    }
    if (command.deliveryState === "terminal") {
      const same = command.status === frame.status && command.resolution === frame.resolution;
      this.send(ws, {
        type: "response",
        requestId: frame.requestId,
        ok: same,
        ...(same ? { command, duplicate: true } : { error: "terminal_conflict" })
      });
      return;
    }
    if (command.deliveryState !== "claimed" || command.claimToken !== frame.claimToken) {
      this.send(ws, { type: "response", requestId: frame.requestId, ok: false, error: "claim_mismatch" });
      return;
    }
    const revision = this.commandRevision() + 1;
    this.ctx.storage.sql.exec(
      "UPDATE commands SET delivery_state = 'terminal', status = ?, resolution = ?, update_revision = ? WHERE command_id = ? AND claim_token = ?",
      frame.status,
      frame.resolution ?? null,
      revision,
      frame.commandId,
      frame.claimToken
    );
    this.setMeta("commandRevision", String(revision));
    this.markBackupDirty();
    const resolved = this.loadCommand(frame.commandId);
    this.send(ws, { type: "response", requestId: frame.requestId, ok: true, command: resolved, duplicate: false });
    if (resolved) this.broadcastCommand(resolved);
  }

  private async submitCommand(
    command: SubmittedHubCommand,
    userId: string
  ): Promise<{ ok: boolean; duplicate?: boolean; conflict?: boolean; command?: StoredHubCommand }> {
    const canonicalJsonValue = canonicalJson(command);
    const canonical = `sha256:${await sha256Hex(canonicalJsonValue)}`;
    const existing = this.loadCommand(command.id);
    if (existing) {
      const row = [...this.ctx.storage.sql.exec("SELECT canonical FROM commands WHERE command_id = ?", command.id)][0];
      if (row?.canonical !== canonical && row?.canonical !== canonicalJsonValue) {
        return { ok: false, conflict: true };
      }
      return { ok: true, duplicate: true, command: existing };
    }
    const sequence = Number(this.getMeta("commandSequence") ?? "0") + 1;
    const revision = this.commandRevision() + 1;
    this.ctx.storage.sql.exec(
      "INSERT INTO commands(seq, command_id, canonical, kind, payload, issued_by, issued_at, expires_at, based_on, update_revision, delivery_state, status, claimed_by, claim_token, resolution) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', 'pending', NULL, NULL, NULL)",
      sequence,
      command.id,
      canonical,
      command.kind,
      JSON.stringify(command.payload),
      command.issuedBy,
      command.issuedAt,
      command.expiresAt,
      command.basedOn === undefined ? null : JSON.stringify(command.basedOn),
      revision
    );
    this.setMeta("commandSequence", String(sequence));
    this.setMeta("commandRevision", String(revision));
    this.markBackupDirty();
    const stored = this.loadCommand(command.id);
    if (stored) {
      this.broadcastHosts({ type: "command", command: stored });
      await this.nudgeHost(userId);
    }
    return { ok: true, duplicate: false, ...(stored ? { command: stored } : {}) };
  }

  private cancelCommand(commandId: string, device: string): boolean {
    const revision = this.commandRevision() + 1;
    const result = this.ctx.storage.sql.exec(
      "UPDATE commands SET delivery_state = 'terminal', status = 'cancelled', resolution = 'cancelled by composer', update_revision = ? WHERE command_id = ? AND issued_by = ? AND delivery_state = 'pending'",
      revision,
      commandId,
      device
    );
    if (result.rowsWritten === 0) return false;
    this.setMeta("commandRevision", String(revision));
    this.markBackupDirty();
    const command = this.loadCommand(commandId);
    if (command) this.broadcastCommand(command);
    return true;
  }

  private loadCommand(commandId: string): StoredHubCommand | undefined {
    const row = [...this.ctx.storage.sql.exec(
      "SELECT seq, command_id, kind, payload, issued_by, issued_at, expires_at, based_on, update_revision, delivery_state, status, claimed_by, claim_token, resolution FROM commands WHERE command_id = ?",
      commandId
    )][0];
    return row ? this.decodeCommand(row) : undefined;
  }

  private actionableCommands(device: string): StoredHubCommand[] {
    const commands: StoredHubCommand[] = [];
    let bytes = 0;
    for (const row of this.ctx.storage.sql.exec(
      "SELECT seq, command_id, kind, payload, issued_by, issued_at, expires_at, based_on, update_revision, delivery_state, status, claimed_by, claim_token, resolution FROM commands WHERE delivery_state = 'pending' OR (delivery_state = 'claimed' AND claimed_by = ?) ORDER BY seq LIMIT 501",
      device
    )) {
      const command = this.decodeCommand(row);
      const commandBytes = new TextEncoder().encode(JSON.stringify(command)).length;
      if (commands.length > 0 && bytes + commandBytes > MAX_COMMAND_PAGE_BYTES) break;
      commands.push(command);
      bytes += commandBytes;
      if (commands.length === 500) break;
    }
    return commands;
  }

  private decodeCommand(row: Record<string, SqlStorageValue>): StoredHubCommand {
    const basedOn = typeof row.based_on === "string" ? JSON.parse(row.based_on) as unknown : undefined;
    return {
      id: row.command_id as string,
      kind: row.kind as string,
      payload: JSON.parse(row.payload as string) as unknown,
      issuedBy: row.issued_by as string,
      issuedAt: row.issued_at as number,
      expiresAt: row.expires_at as number,
      ...(basedOn === undefined ? {} : { basedOn }),
      seq: row.seq as number,
      updateRevision: row.update_revision as number,
      deliveryState: row.delivery_state as StoredHubCommand["deliveryState"],
      status: row.status as StoredHubCommand["status"],
      ...(typeof row.claimed_by === "string" ? { claimedBy: row.claimed_by } : {}),
      ...(typeof row.claim_token === "string" ? { claimToken: row.claim_token } : {}),
      ...(typeof row.resolution === "string" ? { resolution: row.resolution } : {})
    };
  }

  private bootstrap(): {
    sequence: number;
    manifest: unknown;
    pages: unknown[];
    deltas: ProjectionDeltaRow[];
  } {
    const manifest = getJsonBlob<unknown>(this.blobs, "projection-manifest") ?? {
      catalogRevision: "empty",
      totalMessages: 0,
      pages: [],
      turns: []
    };
    const livePage = getJsonBlob<unknown>(this.blobs, "projection-live-base");
    const baseSequence = Number(this.getMeta("projectionBaseSequence") ?? "0");
    const deltas = [...this.ctx.storage.sql.exec(
      "SELECT seq, page_id, page_revision, frame FROM projection_deltas WHERE seq > ? ORDER BY seq",
      baseSequence
    )].map((row): ProjectionDeltaRow => ({
      sequence: row.seq as number,
      delta: {
        pageId: row.page_id as string,
        pageRevision: row.page_revision as string,
        frame: JSON.parse(row.frame as string) as unknown
      }
    }));
    return {
      sequence: baseSequence,
      manifest,
      pages: livePage === undefined ? [] : [livePage],
      deltas
    };
  }

  private sendBootstrap(ws: WebSocket): void {
    const bootstrap = this.bootstrap();
    this.send(ws, {
      type: "bootstrap",
      bootstrap: {
        sequence: bootstrap.sequence,
        manifest: bootstrap.manifest,
        pages: bootstrap.pages
      }
    });
    for (const delta of bootstrap.deltas) {
      this.send(ws, { type: "delta", ...delta });
    }
  }

  private publication(publishId: string): number | undefined {
    const row = [...this.ctx.storage.sql.exec(
      "SELECT seq FROM publications WHERE publish_id = ?",
      publishId
    )][0];
    return row?.seq as number | undefined;
  }

  private recordPublication(publishId: string, sequence: number, kind: "base" | "delta"): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO publications(publish_id, seq, kind, created_at) VALUES (?, ?, ?, ?)",
      publishId,
      sequence,
      kind,
      Date.now()
    );
    this.ctx.storage.sql.exec(
      "DELETE FROM publications WHERE publish_id NOT IN (SELECT publish_id FROM publications ORDER BY created_at DESC LIMIT 1000)"
    );
  }

  private deltaBudget(): { rows: number; bytes: number } {
    const row = [...this.ctx.storage.sql.exec(
      "SELECT COUNT(*) AS rows, COALESCE(SUM(byte_len), 0) AS bytes FROM projection_deltas"
    )][0];
    return { rows: row?.rows as number ?? 0, bytes: row?.bytes as number ?? 0 };
  }

  private stats(): Record<string, unknown> {
    const commands = [...this.ctx.storage.sql.exec(
      "SELECT COUNT(*) AS total, SUM(CASE WHEN delivery_state = 'pending' THEN 1 ELSE 0 END) AS pending, SUM(CASE WHEN delivery_state = 'claimed' THEN 1 ELSE 0 END) AS claimed FROM commands"
    )][0];
    const oldest = [...this.ctx.storage.sql.exec(
      "SELECT MIN(issued_at) AS issued_at FROM commands WHERE delivery_state = 'pending'"
    )][0];
    return {
      hostDevice: this.getMeta("hostDevice") ?? null,
      writerLease: Number(this.getMeta("writerLease") ?? "0"),
      hostConnected: this.ctx.getWebSockets(HOST_TAG).length > 0,
      viewerSockets: this.ctx.getWebSockets(VIEWER_TAG).length,
      projectionSequence: this.projectionSequence(),
      projectionBaseSequence: Number(this.getMeta("projectionBaseSequence") ?? "0"),
      deltaBudget: this.deltaBudget(),
      commandRevision: this.commandRevision(),
      commands: {
        total: commands?.total as number ?? 0,
        pending: commands?.pending as number ?? 0,
        claimed: commands?.claimed as number ?? 0,
        oldestPendingAt: oldest?.issued_at as number | null ?? null
      },
      backupDirty: this.getMeta("backupDirty") === "1",
      backupRevision: Number(this.getMeta("backupRevision") ?? "0"),
      diffPublished: this.blobs.get("diff-v2") !== undefined,
      diffSequence: Number(this.getMeta("diffSequence") ?? "0")
    };
  }

  private async nudgeHost(userId: string): Promise<void> {
    const hostDevice = this.getMeta("hostDevice");
    const chatId = this.getMeta("chatId");
    if (!hostDevice || !chatId) return;
    const room = this.env.DEVICE_ROOMS.get(this.env.DEVICE_ROOMS.idFromName(`d2/${hostDevice}`));
    await room.fetch(new Request("https://device-room/nudge", {
      method: "POST",
      headers: { [AUTH_USER_HEADER]: userId, "content-type": "application/json" },
      body: JSON.stringify({ chatId })
    })).catch(() => undefined);
  }

  private markBackupDirty(): void {
    this.setMeta("backupDirty", "1");
    this.setMeta("backupRevision", String(Number(this.getMeta("backupRevision") ?? "0") + 1));
    void this.ctx.storage.getAlarm().then((existing) => {
      if (existing === null) void this.ctx.storage.setAlarm(Date.now() + DAY_MS);
    });
  }

  async alarm(): Promise<void> {
    if (this.getMeta("retired") === "1") return;
    if (this.getMeta("backupDirty") !== "1") return;
    const chatId = this.getMeta("chatId");
    const owner = this.getMeta("owner");
    if (!chatId || !owner) return;
    const backupRevision = Number(this.getMeta("backupRevision") ?? "0");
    const commands = [...this.ctx.storage.sql.exec(
      "SELECT seq, command_id, canonical, kind, payload, issued_by, issued_at, expires_at, based_on, update_revision, delivery_state, status, claimed_by, claim_token, resolution FROM commands ORDER BY seq"
    )].map((row) => ({
      canonical: row.canonical as string,
      ...this.decodeCommand(row)
    }));
    const backup = {
      version: 1,
      chatId,
      hostDevice: this.getMeta("hostDevice") ?? null,
      projectionSequence: this.projectionSequence(),
      projectionBaseSequence: Number(this.getMeta("projectionBaseSequence") ?? "0"),
      commandRevision: this.commandRevision(),
      manifest: getJsonBlob<unknown>(this.blobs, "projection-manifest") ?? null,
      livePage: getJsonBlob<unknown>(this.blobs, "projection-live-base") ?? null,
      deltas: this.bootstrap().deltas,
      commands,
      diff: getJsonBlob<StoredDiffSidecar>(this.blobs, "diff-v2") ?? null,
      diffSequence: Number(this.getMeta("diffSequence") ?? "0")
    };
    await this.env.BLOBS.put(`hub-backup/${owner}/${chatId}/latest.json`, JSON.stringify(backup), {
      httpMetadata: { contentType: "application/json" }
    });
    if (Number(this.getMeta("backupRevision") ?? "0") === backupRevision) {
      this.setMeta("backupDirty", "0");
    } else {
      await this.ctx.storage.setAlarm(Date.now() + DAY_MS);
    }
  }

  private async retire(): Promise<void> {
    this.setMeta("retired", "1");
    await this.ctx.storage.deleteAlarm();
    for (const socket of this.ctx.getWebSockets()) {
      try { socket.close(4411, "session retired"); } catch { /* closed */ }
    }
    const owner = this.getMeta("owner");
    const chatId = this.getMeta("chatId");
    if (owner && chatId) {
      await this.env.BLOBS.delete(`hub-backup/${owner}/${chatId}/latest.json`);
      let cursor: string | undefined;
      const prefix = `transcript/${owner}/${chatId}/`;
      do {
        const page = await this.env.BLOBS.list({ prefix, cursor, limit: 1000 });
        if (page.objects.length > 0) await this.env.BLOBS.delete(page.objects.map((object) => object.key));
        cursor = page.truncated ? page.cursor : undefined;
      } while (cursor !== undefined);
    }
    this.ctx.storage.sql.exec("DELETE FROM projection_deltas");
    this.ctx.storage.sql.exec("DELETE FROM publications");
    this.ctx.storage.sql.exec("DELETE FROM commands");
    this.blobs.delete("projection-manifest");
    this.blobs.delete("projection-live-base");
    this.blobs.delete("diff-v2");
  }

  private broadcastViewers(value: unknown): void {
    for (const socket of this.ctx.getWebSockets(VIEWER_TAG)) this.send(socket, value);
  }

  private broadcastHosts(value: unknown): void {
    for (const socket of this.ctx.getWebSockets(HOST_TAG)) this.send(socket, value);
  }

  private broadcastCommand(command: StoredHubCommand): void {
    this.broadcastHosts({ type: "commandUpdate", command });
  }

  private send(ws: WebSocket, value: unknown): void {
    try {
      ws.send(JSON.stringify(value));
    } catch {
      try { ws.close(1011, "delivery failed"); } catch { /* closed */ }
    }
  }
}

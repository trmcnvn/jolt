/**
 * RegistryRoom — one Durable Object per per-user workspace registry
 * (`reg1/{orgId}/{userId}`), the wedge-proof replacement for the Loro
 * workspace doc (docs/registry-sync.md).
 *
 * The DO is the authority: it stores CURRENT row state in its SQLite (no
 * update log, no replay, no wasm), applies pushed ops with per-field LWW
 * (registry-core.ts), bumps one monotonic `seq` per accepted batch, and
 * broadcasts merged rows to every connected socket. Cold start is a table
 * read — the 2026-07/08 wedge class (CPU-limited history replay) cannot
 * exist here by construction.
 *
 * Persistence model:
 * - `rows` — the state. Written synchronously inside the message event (write
 *   rate is index-scale: renames, status flips — never transcript traffic).
 * - `meta` — seq counter, gcFloor, per-device push attribution, backup marks.
 * - presence — memory-only by construction (15s client beats, rebroadcast).
 *
 * Hibernation discipline: ZERO wall-clock timers; ping/pong rides the
 * auto-response pair; the daily alarm does tombstone GC + the R2 backup.
 */
import { applyOp, validateOp, type Op, type Row } from "./registry-core";
import { AUTH_USER_HEADER, type Env } from "./env";

const DAY_MS = 24 * 60 * 60 * 1000;
/** Tombstones older than this are purged; cursors from before the purge
 * horizon get a full resync (`gcFloor`). */
const TOMBSTONE_RETAIN_MS = 30 * DAY_MS;
/** Ops per push batch — a cascade delete of a huge space stays comfortably
 * under this; anything larger is split by the client. */
const MAX_BATCH_OPS = 500;
/** Serialized inbound frame budget. */
const MAX_FRAME_BYTES = 1_000_000;
const ARTIFACT_PURGE_BATCH = 20;
const ARTIFACT_PURGE_RETRY_MS = 60_000;

interface SocketState {
  userId: string;
  device: string;
  /** Set once a valid hello established the session. */
  ready?: boolean;
}

interface WireOp extends Op {}

interface PushOutcome {
  ok: number;
  rejected: number;
  lastOkAt: number;
}

interface ArtifactPurgeRow {
  chat_id: string;
  user_id: string;
  attempts: number;
  sweeps: number;
}

export class RegistryRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  /** device → last presence beat (epoch ms). Memory-only. */
  private readonly presence = new Map<string, number>();

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS rows (kind TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL, deleted INTEGER NOT NULL, del_hlc TEXT, fields TEXT NOT NULL, clocks TEXT NOT NULL, PRIMARY KEY (kind, id))"
    );
    ctx.storage.sql.exec("CREATE INDEX IF NOT EXISTS rows_seq ON rows (seq)");
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS artifact_purges (chat_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, attempts INTEGER NOT NULL, sweeps INTEGER NOT NULL, next_attempt_at INTEGER NOT NULL)"
    );
    // Protocol-level keepalive distinguishes actor health from socket health: a
    // pong is runtime-answered and proves nothing about this DO's health.
    // Clients judge liveness by probe frames (crates/sync/src/registry.rs).
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  // ── meta helpers ──────────────────────────────────────────────────────────

  private getMeta(key: string): string | undefined {
    const rows = [...this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key)];
    return rows[0]?.value as string | undefined;
  }

  private setMeta(key: string, value: string): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      key,
      value
    );
  }

  private seq(): number {
    return Number(this.getMeta("seq") ?? "0");
  }

  private gcFloor(): number {
    return Number(this.getMeta("gcFloor") ?? "0");
  }

  // ── row storage ───────────────────────────────────────────────────────────

  private loadRow(kind: string, id: string): Row | undefined {
    const rows = [
      ...this.ctx.storage.sql.exec(
        "SELECT seq, deleted, del_hlc, fields, clocks FROM rows WHERE kind = ? AND id = ?",
        kind,
        id
      )
    ];
    const raw = rows[0];
    if (!raw) return undefined;
    return {
      kind,
      id,
      seq: raw.seq as number,
      deleted: (raw.deleted as number) === 1,
      ...(raw.del_hlc ? { delHlc: raw.del_hlc as string } : {}),
      fields: JSON.parse(raw.fields as string) as Row["fields"],
      clocks: JSON.parse(raw.clocks as string) as Row["clocks"]
    };
  }

  private saveRow(row: Row): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO rows (kind, id, seq, deleted, del_hlc, fields, clocks) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(kind, id) DO UPDATE SET seq = excluded.seq, deleted = excluded.deleted, del_hlc = excluded.del_hlc, fields = excluded.fields, clocks = excluded.clocks",
      row.kind,
      row.id,
      row.seq,
      row.deleted ? 1 : 0,
      row.delHlc ?? null,
      JSON.stringify(row.fields),
      JSON.stringify(row.clocks)
    );
  }

  private rowsSince(cursor: number): Row[] {
    const out: Row[] = [];
    for (const raw of this.ctx.storage.sql.exec(
      "SELECT kind, id, seq, deleted, del_hlc, fields, clocks FROM rows WHERE seq > ? ORDER BY seq",
      cursor
    )) {
      out.push({
        kind: raw.kind as string,
        id: raw.id as string,
        seq: raw.seq as number,
        deleted: (raw.deleted as number) === 1,
        ...(raw.del_hlc ? { delHlc: raw.del_hlc as string } : {}),
        fields: JSON.parse(raw.fields as string) as Row["fields"],
        clocks: JSON.parse(raw.clocks as string) as Row["clocks"]
      });
    }
    return out;
  }

  // ── HTTP surface (only reachable through the authed Worker; org membership
  //    and the per-user room name were enforced there) ──────────────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return json({ error: "unauthenticated" }, 401);

    if (url.pathname === "/ws") {
      const device = url.searchParams.get("device") ?? "";
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const state: SocketState = { userId, device };
      pair[1].serializeAttachment(state);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    if (url.pathname === "/stats" && request.method === "GET") {
      const rowCount = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM rows")][0]
        ?.n as number;
      const tombstones = [
        ...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM rows WHERE deleted = 1")
      ][0]?.n as number;
      const pendingArtifactPurges = [
        ...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM artifact_purges")
      ][0]?.n as number;
      return json({
        seq: this.seq(),
        gcFloor: this.gcFloor(),
        rowCount,
        tombstones,
        pendingArtifactPurges,
        connectedSockets: this.ctx.getWebSockets().length,
        // The ONLY per-device attribution surface — kept from the 2026-08-05
        // incident tooling exposed through registry stats.
        pushOutcomes: JSON.parse(this.getMeta("pushOutcomes") ?? "{}") as Record<string, PushOutcome>,
        lastBackupSeq: Number(this.getMeta("backupSeq") ?? "0"),
        lastGcAt: Number(this.getMeta("lastGcAt") ?? "0")
      });
    }

    if (url.pathname === "/rows" && request.method === "GET") {
      // Repair/inspection read: the full current table.
      return json({ seq: this.seq(), gcFloor: this.gcFloor(), rows: this.rowsSince(0) });
    }

    if (url.pathname === "/reset" && request.method === "POST") {
      // Operator wipe. Recovery is automatic and lossless: every client
      // detects `state.seq < cursor` on its next hello and re-seeds the table
      // from its local rows with their ORIGINAL clocks (registry-core
      // rowToSeedOp) — the ws4 repair recipe, built in.
      this.ctx.storage.sql.exec("DELETE FROM rows");
      this.ctx.storage.sql.exec("DELETE FROM meta");
      for (const ws of this.ctx.getWebSockets()) {
        try {
          ws.close(4410, "registry reset");
        } catch {
          /* already gone */
        }
      }
      return json({ ok: true });
    }

    return json({ error: "not found" }, 404);
  }

  // ── WebSocket protocol ────────────────────────────────────────────────────

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message !== "string") {
      ws.close(1003, "text frames only");
      return;
    }
    if (message.length > MAX_FRAME_BYTES) {
      ws.close(1009, "frame too large");
      return;
    }
    let frame: Record<string, unknown>;
    try {
      frame = JSON.parse(message) as Record<string, unknown>;
    } catch {
      ws.close(1002, "bad json");
      return;
    }
    const state = ws.deserializeAttachment() as SocketState;
    switch (frame.t) {
      case "hello":
        this.handleHello(ws, state, frame);
        return;
      case "push":
        this.handlePush(ws, state, frame);
        return;
      case "presence":
        this.handlePresence(ws, state, frame);
        return;
      case "probe":
        send(ws, { t: "probe-ok", seq: this.seq() });
        return;
      default:
        send(ws, { t: "error", code: "bad_frame", message: `unknown frame: ${String(frame.t)}` });
    }
  }

  async webSocketClose(): Promise<void> {
    /* nothing buffered; rows are written synchronously on push */
  }

  async webSocketError(): Promise<void> {
    /* ditto */
  }

  private handleHello(ws: WebSocket, state: SocketState, frame: Record<string, unknown>): void {
    const cursor = typeof frame.cursor === "number" && frame.cursor >= 0 ? frame.cursor : null;
    if (typeof frame.device === "string" && frame.device.length > 0) {
      state.device = frame.device;
    }
    state.ready = true;
    ws.serializeAttachment(state);
    const seq = this.seq();
    const gcFloor = this.gcFloor();
    // A cursor from before the tombstone-GC horizon may have missed purged
    // deletes; a cursor AHEAD of the server means the server lost state (a
    // reset/wipe) — both force `full`, and the client reacts by replacing
    // (former) or re-seeding the server (latter; see registry.rs).
    const full = cursor === null || cursor < gcFloor || cursor > seq;
    const rows = full ? this.rowsSince(0) : this.rowsSince(cursor);
    send(ws, {
      t: "state",
      seq,
      full,
      gcFloor,
      rows,
      presence: Object.fromEntries(this.presence)
    });
  }

  private handlePush(ws: WebSocket, state: SocketState, frame: Record<string, unknown>): void {
    const batch = typeof frame.batch === "string" ? frame.batch : "";
    if (!state.ready || batch === "" || !Array.isArray(frame.ops)) {
      this.recordPush(state.device, false);
      send(ws, { t: "error", code: "bad_push", message: "hello first / malformed push" });
      return;
    }
    const ops = frame.ops as WireOp[];
    if (ops.length === 0 || ops.length > MAX_BATCH_OPS) {
      this.recordPush(state.device, false);
      send(ws, { t: "error", code: "bad_push", message: `batch of ${ops.length} ops` });
      return;
    }
    for (const op of ops) {
      const invalid = validateOp(op);
      if (invalid) {
        // Reject the WHOLE batch: batches are transactional (cascade deletes)
        // and a client that builds one bad op is a client bug to surface, not
        // to partially apply. Rejections are attributed per device on /stats.
        this.recordPush(state.device, false);
        send(ws, { t: "error", code: "invalid_op", message: `${op.kind}/${op.id}: ${invalid}` });
        return;
      }
    }

    // Apply atomically: DO events are single-threaded and SQLite writes in an
    // event commit together, so a mid-batch crash never persists half a batch.
    const nextSeq = this.seq() + 1;
    const touched = new Map<string, Row>();
    let applied = 0;
    for (const op of ops) {
      const key = `${op.kind} ${op.id}`;
      const before = touched.get(key) ?? this.loadRow(op.kind, op.id);
      const { row, changed } = applyOp(before, op);
      if (!changed || row === undefined) continue;
      applied += 1;
      row.seq = nextSeq;
      touched.set(key, row);
    }
    if (applied > 0) {
      const changedRows = [...touched.values()];
      const deletedChats = changedRows.filter((row) => row.kind === "chats" && row.deleted);
      for (const row of changedRows) this.saveRow(row);
      for (const row of deletedChats) this.enqueueArtifactPurge(row.id, state.userId);
      this.setMeta("seq", String(nextSeq));
      this.markBackupDirty(changedRows.some((row) => row.deleted));
    }
    this.recordPush(state.device, true);
    const seq = applied > 0 ? nextSeq : this.seq();
    if (applied > 0) {
      // Merged full rows to EVERY ready socket (sender included — its op may
      // have lost LWW, and the merged row is the truth it must display).
      // Rows go out BEFORE the ack so the sender's authoritative state is
      // current by the time the ack retires its optimistic pending batch —
      // no between-frames flicker window.
      const rows = [...touched.values()];
      for (const socket of this.ctx.getWebSockets()) {
        const socketState = socket.deserializeAttachment() as SocketState | null;
        if (!socketState?.ready) continue;
        send(socket, { t: "rows", seq, rows });
      }
    }
    send(ws, { t: "ack", batch, seq, applied });
  }

  private handlePresence(ws: WebSocket, state: SocketState, frame: Record<string, unknown>): void {
    if (!state.ready || state.device === "") return;
    const at = typeof frame.at === "number" ? frame.at : Date.now();
    this.presence.set(state.device, at);
    for (const socket of this.ctx.getWebSockets()) {
      if (socket === ws) continue;
      const socketState = socket.deserializeAttachment() as SocketState | null;
      if (!socketState?.ready) continue;
      send(socket, { t: "presence", device: state.device, at });
    }
  }

  private recordPush(device: string, ok: boolean): void {
    const key = device === "" ? "(unknown)" : device;
    const outcomes = JSON.parse(this.getMeta("pushOutcomes") ?? "{}") as Record<string, PushOutcome>;
    const entry = outcomes[key] ?? { ok: 0, rejected: 0, lastOkAt: 0 };
    if (ok) {
      entry.ok += 1;
      entry.lastOkAt = Date.now();
    } else {
      entry.rejected += 1;
    }
    outcomes[key] = entry;
    this.setMeta("pushOutcomes", JSON.stringify(outcomes));
  }

  private scheduleAlarm(at: number): void {
    void this.ctx.storage.getAlarm().then((existing) => {
      if (existing === null || existing > at) void this.ctx.storage.setAlarm(at);
    });
  }

  private markBackupDirty(immediate = false): void {
    this.setMeta("backupDirty", "1");
    this.scheduleAlarm(Date.now() + (immediate ? 100 : DAY_MS));
  }

  private enqueueArtifactPurge(chatId: string, userId: string): void {
    const now = Date.now();
    this.ctx.storage.sql.exec(
      "INSERT INTO artifact_purges (chat_id, user_id, attempts, sweeps, next_attempt_at) VALUES (?, ?, 0, 0, ?) ON CONFLICT(chat_id) DO NOTHING",
      chatId,
      userId,
      now
    );
    this.scheduleAlarm(now + 100);
  }

  private async retireSessionHub(chatId: string, userId: string): Promise<void> {
    const room = this.env.SESSION_HUBS.get(
      this.env.SESSION_HUBS.idFromName(`hub1/${chatId}`)
    );
    const response = await room.fetch(new Request(
      `https://session-hub/retire?chatId=${encodeURIComponent(chatId)}`,
      {
        method: "POST",
        headers: { [AUTH_USER_HEADER]: userId }
      }
    ));
    if (!response.ok && response.status !== 404) {
      throw new Error(`session hub retire failed (${response.status})`);
    }
  }

  private async deleteAttachmentPrefix(chatId: string, userId: string): Promise<void> {
    const prefix = `att/${userId}/${chatId}/`;
    let cursor: string | undefined;
    do {
      const page = await this.env.BLOBS.list({ prefix, cursor, limit: 1000 });
      const keys = page.objects.map((object) => object.key);
      if (keys.length > 0) await this.env.BLOBS.delete(keys);
      cursor = page.truncated ? page.cursor : undefined;
    } while (cursor !== undefined);
  }

  private async processArtifactPurges(): Promise<void> {
    const now = Date.now();
    const rows: ArtifactPurgeRow[] = [];
    for (const raw of this.ctx.storage.sql.exec(
      "SELECT chat_id, user_id, attempts, sweeps FROM artifact_purges WHERE next_attempt_at <= ? ORDER BY next_attempt_at LIMIT ?",
      now,
      ARTIFACT_PURGE_BATCH
    )) {
      if (
        typeof raw.chat_id === "string" &&
        typeof raw.user_id === "string" &&
        typeof raw.attempts === "number" &&
        typeof raw.sweeps === "number"
      ) {
        rows.push({
          chat_id: raw.chat_id,
          user_id: raw.user_id,
          attempts: raw.attempts,
          sweeps: raw.sweeps
        });
      }
    }
    for (const row of rows) {
      try {
        await this.retireSessionHub(row.chat_id, row.user_id);
        await this.deleteAttachmentPrefix(row.chat_id, row.user_id);
        if (row.sweeps === 0) {
          // UploadCommit mirrors asynchronously after committing the host-local
          // file. A second sweep outlives any already-in-flight R2 PUT.
          this.ctx.storage.sql.exec(
            "UPDATE artifact_purges SET sweeps = 1, attempts = 0, next_attempt_at = ? WHERE chat_id = ?",
            Date.now() + ARTIFACT_PURGE_RETRY_MS,
            row.chat_id
          );
        } else {
          this.ctx.storage.sql.exec("DELETE FROM artifact_purges WHERE chat_id = ?", row.chat_id);
        }
      } catch (error) {
        const attempts = row.attempts + 1;
        const delay = Math.min(DAY_MS, ARTIFACT_PURGE_RETRY_MS * 2 ** Math.min(attempts, 8));
        this.ctx.storage.sql.exec(
          "UPDATE artifact_purges SET attempts = ?, next_attempt_at = ? WHERE chat_id = ?",
          attempts,
          Date.now() + delay,
          row.chat_id
        );
        console.error("chat artifact purge failed", `chat=${row.chat_id}`, String(error));
      }
    }
  }

  private scheduleNextArtifactPurge(): void {
    const row = [
      ...this.ctx.storage.sql.exec("SELECT MIN(next_attempt_at) AS at FROM artifact_purges")
    ][0];
    if (typeof row?.at === "number") this.scheduleAlarm(row.at);
  }

  /** Alarm: queued artifact purges, tombstone GC, and registry R2 backup. */
  async alarm(): Promise<void> {
    await this.processArtifactPurges();

    if (this.getMeta("backupDirty") === "1") {
      // Tombstone GC. Raising gcFloor to the purged rows' max seq forces a
      // full resync for any cursor that might have missed a purged delete.
      const horizon = Date.now() - TOMBSTONE_RETAIN_MS;
      const horizonHlc = `${String(horizon).padStart(13, "0")}-`;
      let purgedMaxSeq = 0;
      for (const raw of this.ctx.storage.sql.exec(
        "SELECT seq FROM rows WHERE deleted = 1 AND del_hlc < ?",
        horizonHlc
      )) {
        purgedMaxSeq = Math.max(purgedMaxSeq, raw.seq as number);
      }
      if (purgedMaxSeq > 0) {
        this.ctx.storage.sql.exec("DELETE FROM rows WHERE deleted = 1 AND del_hlc < ?", horizonHlc);
        this.setMeta("gcFloor", String(Math.max(this.gcFloor(), purgedMaxSeq)));
        this.setMeta("lastGcAt", String(Date.now()));
      }

      // Monotonic latest-state backup. Destructive batches schedule this
      // immediately so deleted row fields do not linger until the next day.
      const seq = this.seq();
      if (seq > Number(this.getMeta("backupSeq") ?? "0")) {
        const rows = this.rowsSince(0);
        await this.env.BLOBS.put(
          `backup/registry/${this.ctx.id.toString()}/latest.json`,
          JSON.stringify({ seq, at: Date.now(), rows })
        );
        this.setMeta("backupSeq", String(seq));
      }
      this.setMeta("backupDirty", "0");
    }

    this.scheduleNextArtifactPurge();
  }
}

const send = (ws: WebSocket, frame: Record<string, unknown>): void => {
  try {
    ws.send(JSON.stringify(frame));
  } catch {
    /* socket already gone; hibernation API cleans it up */
  }
};

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

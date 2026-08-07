import { describe, expect, it } from "vitest";
import { CHUNK_BYTES } from "./blobs";
import { appendUpdateRow, ensureUpdateLog, readUpdateRows } from "./update-log";

/** Minimal SqlStorage fake covering exactly the statements update-log.ts
 * issues, including the ~2MB row cap that motivated chunking. */
const ROW_CAP = 2 * 1024 * 1024;

class FakeSql {
  rows: { seq: number; bytes: ArrayBuffer; received_at: number; cont: number }[] = [];
  private seq = 0;
  exec(query: string, ...params: unknown[]): Iterable<Record<string, unknown>> {
    if (query.startsWith("CREATE TABLE")) return [];
    if (query.startsWith("INSERT INTO updates")) {
      const bytes = params[0] as ArrayBuffer;
      if (bytes.byteLength > ROW_CAP) throw new Error("string or blob too big: SQLITE_TOOBIG");
      this.rows.push({
        seq: ++this.seq,
        bytes,
        received_at: params[1] as number,
        cont: (params[2] as number) ?? 0
      });
      return [];
    }
    if (query.startsWith("SELECT bytes, cont FROM updates")) {
      return this.rows.map((r) => ({ bytes: r.bytes, cont: r.cont }));
    }
    throw new Error(`FakeSql: unhandled query: ${query}`);
  }
}

const asSql = (fake: FakeSql) => fake as unknown as SqlStorage;

const bytesOf = (len: number, seed: number): Uint8Array => {
  const out = new Uint8Array(len);
  for (let i = 0; i < len; i++) out[i] = (seed + i * 31) & 0xff;
  return out;
};

const readAll = (fake: FakeSql): Uint8Array[] => [...readUpdateRows(asSql(fake))];

/** Byte-identical check that stays fast on multi-MB arrays (vitest's deep
 * equality walks element-by-element and times out). */
const sameBytes = (a: Uint8Array | undefined, b: Uint8Array): boolean => {
  if (a === undefined || a.byteLength !== b.byteLength) return false;
  for (let index = 0; index < a.byteLength; index++) {
    if (a[index] !== b[index]) return false;
  }
  return true;
};

describe("update log chunking", () => {
  it("stores a small update as a single non-continuation row", () => {
    const sql = new FakeSql();
    const update = bytesOf(1000, 1);
    appendUpdateRow(asSql(sql), update, 42);
    expect(sql.rows.length).toBe(1);
    expect(sql.rows[0]!.cont).toBe(0);
    expect(readAll(sql)).toEqual([update]);
  });

  it("splits an update above the row cap and reassembles it byte-identically", () => {
    const sql = new FakeSql();
    const whale = bytesOf(3 * CHUNK_BYTES + 123, 7); // >2 rows, ragged tail
    appendUpdateRow(asSql(sql), whale, 42);
    expect(sql.rows.length).toBe(4);
    expect(sql.rows.map((r) => r.cont)).toEqual([0, 1, 1, 1]);
    // The bug this guards: every row must fit under the SQL value cap.
    for (const row of sql.rows) expect(row.bytes.byteLength).toBeLessThanOrEqual(ROW_CAP);
    const back = readAll(sql);
    expect(back.length).toBe(1);
    expect(sameBytes(back[0], whale)).toBe(true);
  });

  it("keeps interleaved small and chunked updates in order", () => {
    const sql = new FakeSql();
    const a = bytesOf(10, 1);
    const b = bytesOf(CHUNK_BYTES + 5, 2);
    const c = bytesOf(20, 3);
    for (const u of [a, b, c]) appendUpdateRow(asSql(sql), u, 42);
    const back = readAll(sql);
    expect(back.length).toBe(3);
    expect(sameBytes(back[0], a)).toBe(true);
    expect(sameBytes(back[1], b)).toBe(true);
    expect(sameBytes(back[2], c)).toBe(true);
  });

  it("respects a subarray view's offset when slicing chunks", () => {
    const sql = new FakeSql();
    const backing = bytesOf(CHUNK_BYTES + 200, 9);
    const view = backing.subarray(100, CHUNK_BYTES + 150);
    appendUpdateRow(asSql(sql), view, 42);
    const back = readAll(sql);
    expect(back.length).toBe(1);
    expect(sameBytes(back[0], view)).toBe(true);
  });

  it("ensureUpdateLog is idempotent", () => {
    const sql = new FakeSql();
    ensureUpdateLog(asSql(sql));
    ensureUpdateLog(asSql(sql));
    appendUpdateRow(asSql(sql), bytesOf(10, 1), 1);
    expect(readAll(sql).length).toBe(1);
  });
});

import { describe, expect, it } from "vitest";
import { decodeDeviceFrame, encodeDeviceFrame } from "./device-room";

describe("device frame codec", () => {
  it("round-trips header + payload", () => {
    const payload = new Uint8Array([1, 2, 3, 250, 255]);
    const frame = encodeDeviceFrame({ s: "term-42", k: "term", to: "conn-9" }, payload);
    const decoded = decodeDeviceFrame(frame);
    expect(decoded.header).toEqual({ s: "term-42", k: "term", to: "conn-9" });
    expect([...decoded.payload]).toEqual([...payload]);
  });

  it("handles empty payloads, long headers, and compression capability", () => {
    const header = { s: "x".repeat(200), k: "rpc", from: "conn-1", z: true };
    const decoded = decodeDeviceFrame(encodeDeviceFrame(header, new Uint8Array()));
    expect(decoded.header).toEqual(header);
    expect(decoded.payload.length).toBe(0);
  });

  it("rejects malformed header fields and oversized payloads", () => {
    expect(() => encodeDeviceFrame({ s: "x".repeat(257), k: "rpc" }, new Uint8Array())).toThrow();
    expect(() =>
      encodeDeviceFrame({ s: "rpc", k: "rpc" }, new Uint8Array(8 * 1024 * 1024 + 1))
    ).toThrow();

    const json = new TextEncoder().encode('{"s":1,"k":"rpc"}');
    const malformed = new Uint8Array(1 + json.length);
    malformed[0] = json.length;
    malformed.set(json, 1);
    expect(() => decodeDeviceFrame(malformed)).toThrow();
  });
});

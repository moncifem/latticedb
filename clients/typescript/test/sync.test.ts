// Behaviour of the sync codec that does not depend on the shared vectors: the worked examples of
// the format document (copied, not generated), limits, and input handling specific to JavaScript.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  MAX_PAYLOAD,
  SyncError,
  decode,
  decodeWithLimit,
  encode,
  fromHex,
  toHex,
  type Message,
} from "../src/index.js";
import { thrown } from "./support.js";

const hex = (s: string): Uint8Array => fromHex(s.replace(/[\s|]/g, ""));

test("worked examples of formats/sync-protocol-v1.md", () => {
  const examples: [Message, string][] = [
    [
      { type: "hello", minVersion: 1, maxVersion: 1, agent: "sdk-rs" },
      "01 0000000c | 0001 0001 0006 73646b2d7273",
    ],
    [
      { type: "subscribe", sub: 2, scope: "ws/main", cursor: hex("0102030405060708") },
      "03 00000017 | 00000002 0007 77732f6d61696e 0008 0102030405060708",
    ],
    [
      {
        type: "events",
        sub: 1,
        events: [
          { cursor: hex("0001"), data: hex("61") },
          { cursor: hex("0002"), data: new Uint8Array(0) },
        ],
      },
      "04 00000017 | 00000001 0002 | 0002 0001 00000001 61 | 0002 0002 00000000",
    ],
    [{ type: "pong", nonce: 0x0102030405060708n }, "08 00000008 | 0102030405060708"],
  ];
  for (const [message, bytes] of examples) {
    assert.equal(toHex(encode(message)), toHex(hex(bytes)));
    assert.deepStrictEqual(decode(hex(bytes)), { message, length: hex(bytes).length });
  }
});

test("a frame of exactly the maximum payload round-trips and one byte more is refused", () => {
  // Overhead of an Events frame with one event and a one-byte cursor: 4 + 2 + (2 + 1) + 4.
  const room = MAX_PAYLOAD - 13;
  const at = (n: number): Message => ({
    type: "events",
    sub: 1,
    events: [{ cursor: new Uint8Array([1]), data: new Uint8Array(n).fill(7) }],
  });
  const frame = encode(at(room));
  assert.equal(frame.length, MAX_PAYLOAD + 5);
  assert.deepStrictEqual(decode(frame), { message: at(room), length: frame.length });
  const err = thrown(() => encode(at(room + 1)));
  assert.ok(err instanceof SyncError && err.kind === "too_large");
});

test("a lowered payload limit applies and a higher one is clamped", () => {
  const ping = encode({ type: "ping", nonce: 1n });
  assert.equal((thrown(() => decodeWithLimit(ping, 7)) as SyncError).kind, "bad_length");
  assert.notEqual(decodeWithLimit(ping, 8), null);
  const over = hex("07 00100001");
  assert.equal((thrown(() => decodeWithLimit(over, Number.MAX_SAFE_INTEGER)) as SyncError).detail, 1_048_577);
});

test("the type and the length are judged from the header alone", () => {
  assert.equal((thrown(() => decode(hex("09"))) as SyncError).kind, "unknown_type");
  assert.equal((thrown(() => decode(hex("07 ffffffff"))) as SyncError).kind, "bad_length");
  assert.equal(decode(hex("07 000000")), null);
});

test("decoding works on a view into a larger buffer", () => {
  const frame = encode({ type: "ack", sub: 3, cursor: new Uint8Array([9, 9]) });
  const pool = new Uint8Array(frame.length + 10).fill(0xee);
  pool.set(frame, 7);
  const view = pool.subarray(7, 7 + frame.length);
  assert.deepStrictEqual(decode(view)?.message, { type: "ack", sub: 3, cursor: new Uint8Array([9, 9]) });
  const buffer = Buffer.from(frame);
  assert.equal(decode(buffer)?.length, frame.length);
});

test("a decoded message does not alias the input", () => {
  const frame = encode({ type: "ack", sub: 1, cursor: new Uint8Array([1, 2]) });
  const decoded = decode(frame);
  assert.ok(decoded !== null && decoded.message.type === "ack");
  frame.fill(0);
  assert.deepStrictEqual(decoded.message.cursor, new Uint8Array([1, 2]));
});

test("encoding does not keep a reference to the message", () => {
  const cursor = new Uint8Array([1, 2]);
  const frame = encode({ type: "ack", sub: 1, cursor });
  cursor.fill(0);
  assert.deepStrictEqual(decode(frame)?.message, { type: "ack", sub: 1, cursor: new Uint8Array([1, 2]) });
});

test("text with a lone surrogate cannot be encoded", () => {
  for (const agent of ["\ud800", "a\udc00b", "\ud83d"]) {
    const err = thrown(() => encode({ type: "hello", minVersion: 1, maxVersion: 1, agent }));
    assert.ok(err instanceof SyncError && err.kind === "not_utf8", agent);
  }
  // A well-formed pair is one four-byte character.
  const ok = encode({ type: "hello", minVersion: 1, maxVersion: 1, agent: "😀" });
  assert.equal(decode(ok)?.message.type, "hello");
});

test("numbers outside the field's range are refused instead of wrapping", () => {
  const bad: Message[] = [
    { type: "hello", minVersion: 1, maxVersion: 65_536, agent: "a" },
    { type: "hello", minVersion: 1.5, maxVersion: 2, agent: "a" },
    { type: "hello", minVersion: Number.NaN, maxVersion: 2, agent: "a" },
    { type: "subscribe", sub: 2 ** 32, scope: "s", cursor: new Uint8Array(0) },
    { type: "subscribe", sub: -1, scope: "s", cursor: new Uint8Array(0) },
    { type: "ack", sub: 1.5, cursor: new Uint8Array([1]) },
    { type: "error", code: "internal", sub: 2 ** 32, message: "" },
    { type: "ping", nonce: 1n << 64n },
    { type: "pong", nonce: -1n },
    { type: "welcome", version: 1, maxPayload: 2 ** 32, server: "s" },
  ];
  for (const m of bad) {
    const err = thrown(() => encode(m));
    assert.ok(err instanceof SyncError && err.kind === "invalid_field", JSON.stringify(m, (_, v: unknown) => (typeof v === "bigint" ? String(v) : v)));
  }
});

test("a code outside the defined set cannot be encoded", () => {
  const err = thrown(() => encode({ type: "error", code: "nope" as never, sub: 0, message: "" }));
  assert.ok(err instanceof SyncError && err.kind === "invalid_field");
});

test("a stream of frames decodes one at a time", () => {
  const frames = [
    encode({ type: "ping", nonce: 1n }),
    encode({ type: "pong", nonce: 2n }),
    encode({ type: "ack", sub: 1, cursor: new Uint8Array([5]) }),
  ];
  let stream = new Uint8Array(frames.reduce((n, f) => n + f.length, 0));
  let at = 0;
  for (const f of frames) {
    stream.set(f, at);
    at += f.length;
  }
  const types: string[] = [];
  for (;;) {
    const d = decode(stream);
    if (d === null) {
      break;
    }
    types.push(d.message.type);
    stream = stream.subarray(d.length);
  }
  assert.deepStrictEqual(types, ["ping", "pong", "ack"]);
  assert.equal(stream.length, 0);
});

test("a payload limit that is not a non-negative safe integer is refused", () => {
  const ping = encode({ type: "ping", nonce: 1n });
  for (const limit of [Number.NaN, -1, 1.5, Number.POSITIVE_INFINITY, 2 ** 53]) {
    const err = thrown(() => decodeWithLimit(ping, limit));
    assert.ok(err instanceof SyncError && err.kind === "invalid_field" && err.detail === "limit", String(limit));
  }
  assert.notEqual(decodeWithLimit(ping, 8), null);
});

// Change feed codec behaviour beyond the shared vectors.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CURSOR_LEN,
  FeedDecodeError,
  FeedError,
  MAX_KEY_LEN,
  MAX_VALUE_LEN,
  ORIGIN,
  compareCursors,
  cursorFromBytes,
  cursorToBytes,
  decodeEvent,
  encodeEvent,
  fromHex,
  newCursor,
  toHex,
  type ChangeEvent,
} from "../src/index.js";
import { thrown } from "./support.js";

const hex = (s: string): Uint8Array => fromHex(s.replace(/[\s|]/g, ""));

const sample: ChangeEvent = {
  cursor: { lsn: 258n, index: 1 },
  setLen: 3,
  changeSet: new Uint8Array(16).fill(0xa0),
  hlc: { wallMs: 5n, counter: 6, node: 7 },
  author: 9n,
  turn: null,
  kind: 2,
  key: new TextEncoder().encode("k"),
  op: { type: "upsert", value: new TextEncoder().encode("vv") },
};

// Copied from the worked example in formats/change-feed-v1.md.
const sampleBytes = hex(
  "01 | 0000000000000102 00000001 | 03000000 | a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0 | " +
    "0500000000000000 06000000 07000000 | 00000000000000000000000000000009 | 00 | 02000000 | " +
    "0100 6b | 01 02000000 7676",
);

test("the upsert example of the format document", () => {
  assert.equal(toHex(encodeEvent(sample)), toHex(sampleBytes));
  assert.deepStrictEqual(decodeEvent(sampleBytes), sample);
});

test("a cursor is 12 bytes, big endian, and the origin is all zero", () => {
  assert.equal(CURSOR_LEN, 12);
  assert.equal(toHex(cursorToBytes(ORIGIN)), "000000000000000000000000");
  assert.deepStrictEqual(cursorFromBytes(new Uint8Array(12)), ORIGIN);
});

test("newCursor refuses LSN 0 and out-of-range parts", () => {
  assert.equal((thrown(() => newCursor(0n, 0)) as FeedError).kind, "zero_lsn");
  assert.equal((thrown(() => newCursor(1n << 64n, 0)) as FeedError).kind, "invalid_value");
  assert.equal((thrown(() => newCursor(1n, 2 ** 32)) as FeedError).kind, "invalid_value");
  assert.deepStrictEqual(newCursor(1n, 0), { lsn: 1n, index: 0 });
});

test("cursors order by lsn, then index, with no precision loss above 2^53", () => {
  const a = { lsn: (1n << 53n) + 1n, index: 0 };
  const b = { lsn: (1n << 53n) + 2n, index: 0 };
  assert.equal(compareCursors(a, b), -1);
  assert.equal(compareCursors(b, a), 1);
  assert.equal(compareCursors(a, a), 0);
  assert.equal(compareCursors({ lsn: 7n, index: 1 }, { lsn: 7n, index: 0 }), 1);
  assert.deepStrictEqual(cursorFromBytes(cursorToBytes(a)), a);
});

test("key and value limits are exact", () => {
  const withKey = (n: number): ChangeEvent => ({ ...sample, key: new Uint8Array(n) });
  assert.deepStrictEqual(decodeEvent(encodeEvent(withKey(MAX_KEY_LEN))).key.length, MAX_KEY_LEN);
  assert.equal((thrown(() => encodeEvent(withKey(MAX_KEY_LEN + 1))) as FeedError).kind, "key_too_large");
  const withValue = (n: number): ChangeEvent => ({ ...sample, op: { type: "upsert", value: new Uint8Array(n) } });
  const big = encodeEvent(withValue(MAX_VALUE_LEN));
  const back = decodeEvent(big);
  assert.ok(back.op.type === "upsert" && back.op.value.length === MAX_VALUE_LEN);
  assert.equal((thrown(() => encodeEvent(withValue(MAX_VALUE_LEN + 1))) as FeedError).kind, "value_too_large");
});

test("a huge declared value length is refused before anything is allocated", () => {
  const bytes = sampleBytes.slice();
  new DataView(bytes.buffer).setUint32(74, 0xffff_ffff, true);
  const err = thrown(() => decodeEvent(bytes));
  assert.ok(err instanceof FeedDecodeError && err.kind === "invalid" && err.detail === "value length");
});

test("values above 2^53 are exact", () => {
  const e: ChangeEvent = {
    ...sample,
    cursor: { lsn: (1n << 63n) + 12345n, index: 4_000_000_000 },
    setLen: 4_294_967_295,
    hlc: { wallMs: (1n << 64n) - 1n, counter: 4_294_967_295, node: 1 },
    author: (1n << 128n) - 3n,
    turn: { session: (1n << 60n) + 1n, turn: (1n << 64n) - 1n },
  };
  assert.deepStrictEqual(decodeEvent(encodeEvent(e)), e);
});

test("values outside their field are refused instead of wrapping", () => {
  const bad: ChangeEvent[] = [
    { ...sample, author: 1n << 128n },
    { ...sample, author: -1n },
    { ...sample, kind: -1 },
    { ...sample, kind: 2 ** 32 },
    { ...sample, hlc: { wallMs: 1n << 64n, counter: 0, node: 0 } },
    { ...sample, hlc: { wallMs: 0n, counter: 1.5, node: 0 } },
    { ...sample, changeSet: new Uint8Array(15) },
    { ...sample, turn: { session: -1n, turn: 0n } },
    { ...sample, setLen: 2 ** 32 },
  ];
  for (const e of bad) {
    assert.equal((thrown(() => encodeEvent(e)) as FeedError).kind, "invalid_value");
  }
});

test("a decoded event does not alias the input", () => {
  const bytes = sampleBytes.slice();
  const ev = decodeEvent(bytes);
  bytes.fill(0);
  assert.deepStrictEqual(ev, sample);
});

test("decoding works on a view into a larger buffer", () => {
  const pool = new Uint8Array(sampleBytes.length + 9).fill(0xee);
  pool.set(sampleBytes, 4);
  assert.deepStrictEqual(decodeEvent(pool.subarray(4, 4 + sampleBytes.length)), sample);
});

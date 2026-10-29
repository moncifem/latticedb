// Every vector the Rust side wrote must decode, encode and fail exactly the same way here.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  FeedDecodeError,
  FeedError,
  MAX_PAYLOAD,
  SyncError,
  compareCursors,
  cursorFromBytes,
  cursorToBytes,
  decode,
  decodeEvent,
  decodeWithLimit,
  encode,
  encodeEvent,
  fromHex,
  negotiate,
  toHex,
} from "../src/index.js";
import {
  arr,
  assertErrorMatches,
  big,
  bytesOf,
  eventFromJson,
  loadVectors,
  messageFromJson,
  num,
  obj,
  str,
  thrown,
} from "./support.js";

const vectors = loadVectors();
const sync = obj(vectors["sync"]);
const feed = obj(vectors["feed"]);

function rows(parent: ReturnType<typeof obj>, key: string): ReturnType<typeof obj>[] {
  const list = arr(parent[key]).map(obj);
  assert.ok(list.length > 0, `no vectors under ${key}`);
  return list;
}

test("the vector file has the expected shape", () => {
  assert.equal(vectors["format"], "lattice-conformance-vectors");
  assert.equal(vectors["version"], 1);
});

test("sync: every valid frame decodes to its message and encodes to its bytes", () => {
  for (const v of rows(sync, "valid")) {
    const name = str(v["name"]);
    const bytes = fromHex(str(v["bytes"]));
    const message = messageFromJson(v["message"]);

    const decoded = decode(bytes);
    assert.deepStrictEqual(decoded, { message, length: bytes.length }, `${name}: decode`);
    assert.equal(toHex(encode(message)), toHex(bytes), `${name}: encode`);
    assert.ok(decoded !== null);
    assert.equal(toHex(encode(decoded.message)), toHex(bytes), `${name}: encode(decode(bytes))`);
  }
});

test("sync: a frame followed by more bytes decodes alone and reports its length", () => {
  for (const v of rows(sync, "valid")) {
    const bytes = fromHex(str(v["bytes"]));
    const longer = new Uint8Array(bytes.length + 3);
    longer.set(bytes);
    longer.set([0xff, 0x00, 0x01], bytes.length);
    const decoded = decode(longer);
    assert.equal(decoded?.length, bytes.length, str(v["name"]));
  }
});

test("sync: every proper prefix of a valid frame is incomplete", () => {
  for (const v of rows(sync, "valid")) {
    const bytes = fromHex(str(v["bytes"]));
    for (let cut = 0; cut < bytes.length; cut++) {
      assert.equal(decode(bytes.subarray(0, cut)), null, `${str(v["name"])}: prefix ${cut}`);
    }
  }
});

test("sync: invalid frames fail with the error Rust reports", () => {
  for (const v of rows(sync, "invalid")) {
    const name = str(v["name"]);
    const limit = v["limit"] === undefined ? MAX_PAYLOAD : num(v["limit"]);
    const err = thrown(() => decodeWithLimit(fromHex(str(v["bytes"])), limit));
    assert.ok(err instanceof SyncError, `${name}: ${String(err)}`);
    assertErrorMatches(err, v["error"], name);
  }
});

test("sync: incomplete input returns null", () => {
  for (const v of rows(sync, "incomplete")) {
    assert.equal(decode(fromHex(str(v["bytes"]))), null, str(v["name"]));
  }
});

test("sync: the encoder refuses what Rust refuses, with the same error", () => {
  for (const v of rows(sync, "encode_errors")) {
    const name = str(v["name"]);
    const err = thrown(() => encode(messageFromJson(v["message"])));
    assert.ok(err instanceof SyncError, `${name}: ${String(err)}`);
    assertErrorMatches(err, v["error"], name);
  }
});

test("sync: version negotiation gives Rust's answer", () => {
  for (const v of rows(sync, "negotiate")) {
    const min = num(v["min"]);
    const max = num(v["max"]);
    const want = obj(v["result"]);
    const got = negotiate(min, max);
    if (want["version"] !== undefined) {
      assert.deepStrictEqual(got, { ok: true, version: num(want["version"]) }, `${min}..=${max}`);
    } else {
      assert.deepStrictEqual(got, { ok: false, error: str(want["error"]) }, `${min}..=${max}`);
    }
  }
});

test("sync: the vectors cover every message type and every error kind", () => {
  const types = new Set(rows(sync, "valid").map((v) => str(obj(v["message"])["type"])));
  assert.deepStrictEqual(
    [...types].sort(),
    ["ack", "error", "events", "hello", "ping", "pong", "subscribe", "welcome"],
  );
  const kinds = new Set(rows(sync, "invalid").map((v) => str(obj(v["error"])["kind"])));
  assert.deepStrictEqual(
    [...kinds].sort(),
    ["bad_length", "invalid_field", "not_utf8", "trailing", "truncated", "unknown_type"],
  );
  const encodeKinds = new Set(rows(sync, "encode_errors").map((v) => str(obj(v["error"])["kind"])));
  assert.ok(encodeKinds.has("too_large") && encodeKinds.has("invalid_field"));
});

test("sync: an Events frame can carry a feed event as its data", () => {
  const v = rows(sync, "valid").find((r) => str(r["name"]) === "events_carrying_a_feed_event");
  assert.ok(v !== undefined);
  const decoded = decode(fromHex(str(v["bytes"])));
  assert.ok(decoded !== null && decoded.message.type === "events");
  const [event] = decoded.message.events;
  assert.ok(event !== undefined);
  const inner = decodeEvent(event.data);
  assert.deepStrictEqual(cursorToBytes(inner.cursor), event.cursor);
});

test("feed: cursors encode and decode as Rust does, and bytes order like cursors", () => {
  const list = rows(feed, "cursors");
  for (const v of list) {
    const name = str(v["name"]);
    const c = obj(v["cursor"]);
    const cursor = { lsn: big(c["lsn"]), index: num(c["index"]) };
    const bytes = fromHex(str(v["bytes"]));
    assert.equal(toHex(cursorToBytes(cursor)), toHex(bytes), `${name}: encode`);
    assert.deepStrictEqual(cursorFromBytes(bytes), cursor, `${name}: decode`);
  }
  for (const a of list) {
    for (const b of list) {
      const ca = cursorFromBytes(bytesOf(a["bytes"]));
      const cb = cursorFromBytes(bytesOf(b["bytes"]));
      const byBytes = Buffer.compare(bytesOf(a["bytes"]), bytesOf(b["bytes"]));
      assert.equal(Math.sign(byBytes), compareCursors(ca, cb), `${str(a["name"])} vs ${str(b["name"])}`);
    }
  }
});

test("feed: invalid cursors fail with Rust's error", () => {
  for (const v of rows(feed, "cursors_invalid")) {
    const name = str(v["name"]);
    const err = thrown(() => cursorFromBytes(fromHex(str(v["bytes"]))));
    assert.ok(err instanceof FeedDecodeError, `${name}: ${String(err)}`);
    assertErrorMatches(err, v["error"], name);
  }
});

test("feed: valid events decode to their value and encode to their bytes", () => {
  for (const v of rows(feed, "events")) {
    const name = str(v["name"]);
    const bytes = fromHex(str(v["bytes"]));
    const event = eventFromJson(v["event"]);
    assert.deepStrictEqual(decodeEvent(bytes), event, `${name}: decode`);
    assert.equal(toHex(encodeEvent(event)), toHex(bytes), `${name}: encode`);
    assert.equal(toHex(encodeEvent(decodeEvent(bytes))), toHex(bytes), `${name}: round trip`);
  }
});

test("feed: every proper prefix of a valid event is truncated", () => {
  for (const v of rows(feed, "events")) {
    const bytes = fromHex(str(v["bytes"]));
    for (let cut = 0; cut < bytes.length; cut++) {
      const err = thrown(() => decodeEvent(bytes.subarray(0, cut)));
      assert.ok(err instanceof FeedDecodeError && err.kind === "truncated", `${str(v["name"])}: prefix ${cut}`);
    }
  }
});

test("feed: invalid events fail with Rust's error", () => {
  for (const v of rows(feed, "events_invalid")) {
    const name = str(v["name"]);
    const err = thrown(() => decodeEvent(fromHex(str(v["bytes"]))));
    assert.ok(err instanceof FeedDecodeError, `${name}: ${String(err)}`);
    assertErrorMatches(err, v["error"], name);
  }
});

test("feed: the encoder refuses what Rust refuses", () => {
  for (const v of rows(feed, "encode_errors")) {
    const name = str(v["name"]);
    const err = thrown(() => encodeEvent(eventFromJson(v["event"])));
    assert.ok(err instanceof FeedError, `${name}: ${String(err)}`);
    assertErrorMatches(err, v["error"], name);
  }
});

// Randomised properties with a seeded PRNG. Case `i` uses seed `i + 1`, so a failure message names
// the seed and the case replays exactly. `LATTICE_TS_CASES` raises the count.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  ERROR_CODE_NAMES,
  FeedDecodeError,
  MAX_CURSOR_LEN,
  MAX_EVENTS_PER_FRAME,
  SyncClient,
  SyncError,
  compareCursors,
  cursorFromBytes,
  cursorToBytes,
  decode,
  decodeEvent,
  defaultConfig,
  encode,
  encodeEvent,
  toHex,
  type Action,
  type ChangeEvent,
  type Message,
} from "../src/index.js";
import { Rng, cases } from "./support.js";

function forEachCase(count: number, fn: (rng: Rng) => void): void {
  for (let i = 0; i < count; i++) {
    const seed = i + 1;
    try {
      fn(new Rng(seed));
    } catch (e) {
      if (e instanceof Error) {
        e.message = `seed ${seed}: ${e.message}`;
      }
      throw e;
    }
  }
}

const encoder = new TextEncoder();
const POOL = ["a", "b", "z", "0", "/", "-", "_", " ", "~", "é", "漢", "\u{1f600}", " ", " ", "﻿", "￾"];

/** Valid text of `min..=max` UTF-8 bytes, no control characters. */
function text(rng: Rng, min: number, max: number): string {
  const want = rng.range(min, Math.min(max, min + 12));
  let s = "";
  while (encoder.encode(s).length < want) {
    const next = s + rng.pick(POOL);
    if (encoder.encode(next).length > max) {
      break;
    }
    s = next;
  }
  while (encoder.encode(s).length < min) {
    s += "a";
  }
  return s;
}

function message(rng: Rng): Message {
  const sub = (): number => rng.range(1, 0xffff_ffff);
  switch (rng.int(8)) {
    case 0: {
      const min = rng.range(1, 0xffff);
      return { type: "hello", minVersion: min, maxVersion: rng.range(min, 0xffff), agent: text(rng, 1, 64) };
    }
    case 1:
      return { type: "welcome", version: 1, maxPayload: rng.range(1024, 1 << 20), server: text(rng, 1, 64) };
    case 2:
      return { type: "subscribe", sub: sub(), scope: text(rng, 1, 512), cursor: rng.bytes(rng.range(0, MAX_CURSOR_LEN)) };
    case 3:
      return {
        type: "events",
        sub: sub(),
        events: Array.from({ length: rng.range(1, rng.int(8) === 0 ? MAX_EVENTS_PER_FRAME : 6) }, () => ({
          cursor: rng.bytes(rng.range(1, MAX_CURSOR_LEN)),
          data: rng.bytes(rng.range(0, 40)),
        })),
      };
    case 4:
      return { type: "ack", sub: sub(), cursor: rng.bytes(rng.range(1, MAX_CURSOR_LEN)) };
    case 5:
      return { type: "error", code: rng.pick(ERROR_CODE_NAMES), sub: rng.u32(), message: text(rng, 0, 256) };
    case 6:
      return { type: "ping", nonce: rng.u64() };
    default:
      return { type: "pong", nonce: rng.u64() };
  }
}

/** Flips, inserts, deletes or truncates bytes, and often repairs the length so the payload parser runs. */
function mutate(rng: Rng, original: Uint8Array, headerLen: number): Uint8Array {
  let b = Array.from(original);
  const edits = rng.range(1, 3);
  for (let i = 0; i < edits; i++) {
    const at = rng.int(Math.max(1, b.length));
    switch (rng.int(6)) {
      case 0:
        b[at] = (b[at] ?? 0) ^ (1 << rng.int(8));
        break;
      case 1:
        b[at] = rng.pick([0, 1, 0x7f, 0x80, 0xc0, 0xed, 0xf4, 0xff]);
        break;
      case 2:
        b.splice(at, 1);
        break;
      case 3:
        b.splice(at, 0, rng.int(256));
        break;
      case 4:
        b = b.slice(0, at);
        break;
      default:
        b.push(rng.int(256));
    }
  }
  if (headerLen > 0 && b.length >= headerLen && rng.int(10) < 7) {
    const len = b.length - headerLen;
    const lenBytes = [(len >>> 24) & 0xff, (len >>> 16) & 0xff, (len >>> 8) & 0xff, len & 0xff];
    b.splice(1, 4, ...lenBytes);
  }
  return Uint8Array.from(b);
}

/** Whatever the input, the decoder returns, waits or throws SyncError; what it accepts is canonical. */
function checkSyncInput(bytes: Uint8Array): void {
  let decoded;
  try {
    decoded = decode(bytes);
  } catch (e) {
    assert.ok(e instanceof SyncError, `unexpected ${String(e)} for ${toHex(bytes)}`);
    return;
  }
  if (decoded === null) {
    return;
  }
  assert.ok(decoded.length <= bytes.length);
  assert.equal(toHex(encode(decoded.message)), toHex(bytes.subarray(0, decoded.length)), "not canonical");
}

test("sync: encode then decode returns the message, and the bytes are canonical", () => {
  forEachCase(cases(3000), (rng) => {
    const m = message(rng);
    const bytes = encode(m);
    assert.deepStrictEqual(decode(bytes), { message: m, length: bytes.length });
    assert.equal(toHex(encode(decode(bytes)!.message)), toHex(bytes));
  });
});

test("sync: a stream of frames split at random points decodes to the same messages", () => {
  forEachCase(cases(500), (rng) => {
    const msgs = Array.from({ length: rng.range(1, 6) }, () => message(rng));
    const frames = msgs.map((m) => encode(m));
    const all = new Uint8Array(frames.reduce((n, f) => n + f.length, 0));
    let at = 0;
    for (const f of frames) {
      all.set(f, at);
      at += f.length;
    }
    // Feed it in random chunks, the way a socket would.
    const got: Message[] = [];
    let pending = new Uint8Array(0);
    for (let pos = 0; pos < all.length; ) {
      const n = rng.range(1, 64);
      const chunk = all.subarray(pos, pos + n);
      pos += n;
      const joined = new Uint8Array(pending.length + chunk.length);
      joined.set(pending);
      joined.set(chunk, pending.length);
      pending = joined;
      for (;;) {
        const d = decode(pending);
        if (d === null) {
          break;
        }
        got.push(d.message);
        pending = pending.subarray(d.length);
      }
    }
    assert.equal(pending.length, 0);
    assert.deepStrictEqual(got, msgs);
  });
});

test("sync: mutated frames never crash the decoder and what it accepts is canonical", () => {
  forEachCase(cases(6000), (rng) => {
    checkSyncInput(mutate(rng, encode(message(rng)), 5));
  });
});

test("sync: random bytes never crash the decoder", () => {
  forEachCase(cases(3000), (rng) => {
    const body = rng.bytes(rng.range(0, 40));
    const header = Uint8Array.from([rng.range(0, 10), 0, 0, 0, body.length]);
    checkSyncInput(rng.bool() ? rng.bytes(rng.range(0, 60)) : Uint8Array.from([...header, ...body]));
  });
});

function changeEvent(rng: Rng): ChangeEvent {
  const lsn = rng.u64() || 1n;
  const setLen = rng.u32() || 1;
  const hex = (): bigint => (rng.u64() << 64n) | rng.u64();
  return {
    cursor: { lsn, index: rng.int(4) === 0 ? setLen - 1 : rng.int(setLen) },
    setLen,
    changeSet: rng.bytes(16),
    hlc: { wallMs: rng.u64(), counter: rng.u32(), node: rng.u32() },
    author: hex(),
    turn: rng.bool() ? null : { session: rng.u64(), turn: rng.u64() },
    kind: rng.u32(),
    key: rng.bytes(rng.range(0, 40)),
    op: rng.bool() ? { type: "delete" } : { type: "upsert", value: rng.bytes(rng.range(0, 40)) },
  };
}

test("feed: encode then decode returns the event, and the bytes are canonical", () => {
  forEachCase(cases(3000), (rng) => {
    const e = changeEvent(rng);
    const bytes = encodeEvent(e);
    assert.deepStrictEqual(decodeEvent(bytes), e);
    assert.equal(toHex(encodeEvent(decodeEvent(bytes))), toHex(bytes));
  });
});

test("feed: mutated events never crash the decoder and what it accepts is canonical", () => {
  forEachCase(cases(6000), (rng) => {
    const bytes = mutate(rng, encodeEvent(changeEvent(rng)), 0);
    try {
      const e = decodeEvent(bytes);
      assert.equal(toHex(encodeEvent(e)), toHex(bytes), "not canonical");
    } catch (err) {
      assert.ok(err instanceof FeedDecodeError, `unexpected ${String(err)} for ${toHex(bytes)}`);
    }
  });
});

test("feed: cursor bytes order exactly like cursors", () => {
  forEachCase(cases(3000), (rng) => {
    const pick = (): { lsn: bigint; index: number } => {
      // Few distinct values, so equal lsn and equal index both come up often.
      const lsn = rng.pick([1n, 2n, 255n, 256n, (1n << 53n) + 1n, (1n << 64n) - 1n]);
      return { lsn, index: rng.pick([0, 1, 255, 256, 0xffff_ffff]) };
    };
    const a = pick();
    const b = pick();
    const byBytes = Buffer.compare(cursorToBytes(a), cursorToBytes(b));
    assert.equal(Math.sign(byBytes), compareCursors(a, b));
    assert.deepStrictEqual(cursorFromBytes(cursorToBytes(a)), a);
  });
});

// ---- client against a model server --------------------------------------------------------------

const cur = (n: number): Uint8Array => {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n), false);
  return b;
};
const posOf = (c: Uint8Array): number => Number(new DataView(c.buffer, c.byteOffset).getBigUint64(0, false));

type Server = "honest" | "skips_one";

/** Positions the application received, in order. The script drives sends, acks and reconnects. */
function run(server: Server, script: readonly (readonly [number, number])[]): number[] {
  const c = new SyncClient({ ...defaultConfig("sdk-ts"), ackBatch: 2 });
  c.connect(0n);
  c.handle({ type: "welcome", version: 1, maxPayload: 1 << 20, server: "s" }, 0n);
  const { sub } = c.subscribe("s", new Uint8Array(0));
  const seen: number[] = [];
  let next = 1;
  let now = 1n;
  for (const [op, arg] of script) {
    now += 1n;
    switch (op % 4) {
      case 0:
      case 1: {
        const n = 1 + (arg % 4);
        const out = c.handle(
          {
            type: "events",
            sub,
            events: Array.from({ length: n }, (_, i) => ({ cursor: cur(next + i), data: new Uint8Array([i]) })),
          },
          now,
        );
        for (const a of out) {
          if (a.type === "deliver") {
            seen.push(posOf(a.cursor));
          }
        }
        next += n;
        break;
      }
      case 2: {
        const last = seen[seen.length - 1];
        if (last !== undefined) {
          c.ack(sub, cur(last));
          c.flushAcks();
        }
        break;
      }
      default: {
        c.disconnected();
        c.reconnect(now);
        const out: Action[] = c.handle({ type: "welcome", version: 1, maxPayload: 1 << 20, server: "s" }, now);
        const resub = out.find((a) => a.type === "send" && a.frame.type === "subscribe");
        assert.ok(resub !== undefined && resub.type === "send" && resub.frame.type === "subscribe");
        const cursor = resub.frame.cursor;
        const acked = cursor.length === 0 ? 0 : posOf(cursor);
        next = server === "honest" ? Math.max(1, acked + 1 - (arg % 3)) : acked + 2;
      }
    }
  }
  return seen;
}

function exactlyOnce(seen: readonly number[]): string | null {
  for (let i = 0; i < seen.length; i++) {
    if (seen[i] !== i + 1) {
      return `position ${i}: got ${String(seen[i])}, want ${i + 1}`;
    }
  }
  return null;
}

test("client: a server that skips an event makes the property fail", () => {
  // Deliver 1 and 2, ack, reconnect: the buggy server resumes at 4 instead of 3.
  const script = [[0, 1], [2, 0], [3, 0], [0, 1]] as const;
  const seen = run("skips_one", script);
  assert.deepStrictEqual(seen, [1, 2, 4, 5]);
  assert.notEqual(exactlyOnce(seen), null);
  assert.equal(exactlyOnce(run("honest", script)), null);
});

test("client: with a server that honours cursors the application sees each event exactly once", () => {
  forEachCase(cases(1500), (rng) => {
    const script = Array.from({ length: rng.range(0, 80) }, () => [rng.int(256), rng.int(256)] as const);
    assert.equal(exactlyOnce(run("honest", script)), null);
  });
});

test("client: arbitrary frames never throw anything but a ClientError", () => {
  forEachCase(cases(1000), (rng) => {
    const c = new SyncClient({ ...defaultConfig("x"), ackBatch: 2 });
    c.connect(0n);
    for (let i = 0; i < rng.int(40); i++) {
      const now = BigInt(i * 7);
      const sub = rng.int(3);
      const first = rng.int(5);
      const frame: Message = ((): Message => {
        switch (rng.int(7)) {
          case 0:
            return { type: "welcome", version: rng.int(3), maxPayload: 1 << 20, server: "s" };
          case 1:
            return {
              type: "events",
              sub,
              events: [0, 1, 2].map((k) => ({ cursor: cur(first + k), data: new Uint8Array(0) })),
            };
          case 2:
            return { type: "error", code: rng.pick(ERROR_CODE_NAMES), sub, message: "" };
          case 3:
            return { type: "ping", nonce: rng.u64() };
          case 4:
            return { type: "pong", nonce: rng.u64() };
          case 5:
            return { type: "ack", sub, cursor: new Uint8Array(0) };
          default:
            return { type: "hello", minVersion: 1, maxVersion: 1, agent: "x" };
        }
      })();
      for (const step of [
        () => c.handle(frame, now),
        () => c.tick(now),
        () => c.subscribe("s", new Uint8Array(0)),
        () => (c.phase() === "disconnected" ? c.reconnect(now) : []),
      ]) {
        try {
          step();
        } catch (e) {
          assert.equal((e as Error).name, "ClientError", String(e));
        }
      }
    }
  });
});

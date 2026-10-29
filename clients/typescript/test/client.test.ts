// The client state machine, case by case: the same scenarios as the tests of
// `lattice_sdk::client` in the Rust SDK.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  ClientError,
  SyncClient,
  defaultConfig,
  type Action,
  type ClientConfig,
  type Message,
  type SyncEvent,
} from "../src/index.js";
import { thrown } from "./support.js";

const cur = (n: number): Uint8Array => {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n), false);
  return b;
};

const ev = (n: number): SyncEvent => ({ cursor: cur(n), data: new Uint8Array([n & 0xff]) });

const events = (sub: number, from: number, to: number): Message => ({
  type: "events",
  sub,
  events: Array.from({ length: to - from + 1 }, (_, i) => ev(from + i)),
});

const welcome = (version: number): Message => ({
  type: "welcome",
  version,
  maxPayload: 1 << 20,
  server: "srv",
});

const delivered = (actions: Action[]): number[] =>
  actions.flatMap((a) => (a.type === "deliver" ? [Number(new DataView(a.cursor.buffer).getBigUint64(0, false))] : []));

function client(batch: number): SyncClient {
  return new SyncClient({ ...defaultConfig("sdk-ts"), ackBatch: batch });
}

function ready(c: SyncClient): void {
  c.connect(0n);
  c.handle(welcome(1), 0n);
}

const send = (frame: Message): Action => ({ type: "send", frame });

test("happy path", () => {
  const c = client(1);
  assert.deepStrictEqual(c.connect(0n), [send({ type: "hello", minVersion: 1, maxVersion: 1, agent: "sdk-ts" })]);
  assert.equal(c.phase(), "hello_sent");
  assert.deepStrictEqual(c.handle(welcome(1), 1n), [{ type: "ready", version: 1, maxPayload: 1 << 20 }]);
  assert.equal(c.phase(), "ready");
  const { sub, actions } = c.subscribe("ws/main", new Uint8Array(0));
  assert.equal(sub, 1);
  assert.deepStrictEqual(actions, [send({ type: "subscribe", sub: 1, scope: "ws/main", cursor: new Uint8Array(0) })]);
  assert.equal(c.phase(), "subscribed");
  assert.deepStrictEqual(delivered(c.handle(events(1, 1, 3), 2n)), [1, 2, 3]);
  assert.deepStrictEqual(c.ack(1, cur(3)), [send({ type: "ack", sub: 1, cursor: cur(3) })]);
  assert.deepStrictEqual(c.resumeCursor(1), cur(3));
});

test("subscribing before the Welcome is refused", () => {
  const c = client(1);
  assert.deepStrictEqual(thrown(() => c.subscribe("s", new Uint8Array(0))), new ClientError("wrong_phase", "disconnected"));
  c.connect(0n);
  assert.deepStrictEqual(thrown(() => c.subscribe("s", new Uint8Array(0))), new ClientError("wrong_phase", "hello_sent"));
});

test("a client newer than the server negotiates down", () => {
  const c = new SyncClient({ ...defaultConfig("a"), maxVersion: 3 });
  const hello = c.connect(0n);
  assert.deepStrictEqual(hello, [send({ type: "hello", minVersion: 1, maxVersion: 3, agent: "a" })]);
  c.handle(welcome(1), 0n);
  assert.equal(c.phase(), "ready");
  assert.equal(c.version(), 1);
});

test("a Welcome outside the offered range is incompatible and terminal", () => {
  const c = client(1);
  c.connect(0n);
  assert.deepStrictEqual(c.handle(welcome(2), 0n), [{ type: "close", reason: { kind: "incompatible" } }]);
  assert.equal(c.phase(), "closed");
  assert.deepStrictEqual(thrown(() => c.reconnect(1n)), new ClientError("wrong_phase", "closed"));
});

test("an unsupported_version error is terminal", () => {
  const c = client(1);
  c.connect(0n);
  const out = c.handle({ type: "error", code: "unsupported_version", sub: 0, message: "version" }, 0n);
  assert.deepStrictEqual(out, [{ type: "close", reason: { kind: "incompatible" } }]);
  assert.equal(c.phase(), "closed");
});

test("a reconnect resumes from the last acked cursor and drops replayed duplicates", () => {
  const c = client(1);
  ready(c);
  const { sub } = c.subscribe("ws/main", new Uint8Array(0));
  assert.deepStrictEqual(delivered(c.handle(events(sub, 1, 5), 1n)), [1, 2, 3, 4, 5]);
  c.ack(sub, cur(3));
  c.disconnected();
  assert.equal(c.phase(), "disconnected");
  c.reconnect(10n);
  const a = c.handle(welcome(1), 10n);
  assert.deepStrictEqual(a[1], send({ type: "subscribe", sub, scope: "ws/main", cursor: cur(3) }));
  assert.equal(c.phase(), "subscribed");
  // The server replays 4 and 5 (already delivered) and then continues.
  assert.deepStrictEqual(delivered(c.handle(events(sub, 4, 7), 11n)), [6, 7]);
  // A full duplicate batch yields nothing.
  assert.deepStrictEqual(delivered(c.handle(events(sub, 1, 7), 12n)), []);
});

test("ack batching and flushing", () => {
  const c = client(3);
  ready(c);
  const { sub } = c.subscribe("s", new Uint8Array(0));
  c.handle(events(sub, 1, 4), 0n);
  assert.deepStrictEqual(c.ack(sub, cur(1)), []);
  assert.deepStrictEqual(c.ack(sub, cur(2)), []);
  assert.deepStrictEqual(c.ack(sub, cur(3)), [send({ type: "ack", sub, cursor: cur(3) })]);
  assert.deepStrictEqual(c.ack(sub, cur(4)), []);
  assert.deepStrictEqual(c.resumeCursor(sub), cur(3));
  assert.deepStrictEqual(c.flushAcks(), [send({ type: "ack", sub, cursor: cur(4) })]);
  assert.deepStrictEqual(c.flushAcks(), []);
  assert.deepStrictEqual(thrown(() => c.ack(sub, cur(9))), new ClientError("ack_beyond_delivered"));
  assert.deepStrictEqual(thrown(() => c.ack(9, cur(1))), new ClientError("unknown_subscription", 9));
});

test("an acknowledgement given while disconnected still moves the resume point", () => {
  const c = client(1);
  ready(c);
  const { sub } = c.subscribe("s", new Uint8Array(0));
  c.handle(events(sub, 1, 3), 0n);
  c.disconnected();
  assert.deepStrictEqual(c.ack(sub, cur(2)), []);
  assert.deepStrictEqual(c.resumeCursor(sub), cur(2));
});

test("a subscription error removes only that subscription", () => {
  const c = client(1);
  ready(c);
  const { sub: a } = c.subscribe("a", new Uint8Array(0));
  const { sub: b } = c.subscribe("b", new Uint8Array(0));
  const out = c.handle({ type: "error", code: "invalid_scope", sub: a, message: "no" }, 0n);
  assert.deepStrictEqual(out, [{ type: "subscription_failed", sub: a, code: "invalid_scope", message: "no" }]);
  assert.deepStrictEqual(c.subscriptions(), [b]);
  assert.equal(c.phase(), "subscribed");
  c.handle({ type: "error", code: "invalid_cursor", sub: b, message: "" }, 0n);
  assert.equal(c.phase(), "ready");
  // An error for a subscription that is gone is ignored.
  assert.deepStrictEqual(c.handle({ type: "error", code: "internal", sub: 77, message: "" }, 0n), []);
});

test("a connection error drops the connection but allows a reconnect", () => {
  const c = client(1);
  ready(c);
  const out = c.handle({ type: "error", code: "internal", sub: 0, message: "boom" }, 0n);
  assert.deepStrictEqual(out, [{ type: "close", reason: { kind: "server_error", code: "internal", message: "boom" } }]);
  assert.equal(c.phase(), "disconnected");
  assert.doesNotThrow(() => c.reconnect(1n));
});

test("protocol violations send malformed and drop", () => {
  const c = client(1);
  ready(c);
  const out = c.handle(welcome(1), 0n);
  assert.deepStrictEqual(out[0], send({ type: "error", code: "malformed", sub: 0, message: "welcome outside handshake" }));
  assert.deepStrictEqual(out[1], { type: "close", reason: { kind: "protocol_violation", rule: "welcome outside handshake" } });
  assert.equal(c.phase(), "disconnected");
  assert.throws(() => c.handle(welcome(1), 0n), ClientError);
});

test("events before the Welcome and client-to-server frames are violations", () => {
  const c = client(1);
  c.connect(0n);
  assert.equal(c.handle(events(1, 1, 1), 0n)[1]?.type, "close");
  c.reconnect(1n);
  const out = c.handle({ type: "ack", sub: 1, cursor: new Uint8Array([1]) }, 1n);
  assert.deepStrictEqual(out[1], { type: "close", reason: { kind: "protocol_violation", rule: "client-to-server frame received" } });
});

test("a server Ping is answered with the same nonce", () => {
  const c = client(1);
  ready(c);
  assert.deepStrictEqual(c.handle({ type: "ping", nonce: 77n }, 1n), [send({ type: "pong", nonce: 77n })]);
});

test("a ping goes out when idle and an unanswered one times out", () => {
  const c = client(1);
  ready(c);
  assert.deepStrictEqual(c.tick(29n), []);
  assert.deepStrictEqual(c.tick(30n), [send({ type: "ping", nonce: 1n })]);
  assert.deepStrictEqual(c.tick(39n), []);
  assert.deepStrictEqual(c.tick(40n), [{ type: "close", reason: { kind: "ping_timeout" } }]);
  assert.equal(c.phase(), "disconnected");
});

test("a matching Pong clears the ping and a wrong nonce does not", () => {
  const c = client(1);
  ready(c);
  c.tick(30n);
  c.handle({ type: "pong", nonce: 99n }, 31n);
  c.handle({ type: "pong", nonce: 1n }, 32n);
  assert.deepStrictEqual(c.tick(40n), []);
  assert.equal(c.phase(), "ready");
  // The next ping is due 30 ticks after the last frame.
  assert.deepStrictEqual(c.tick(62n), [send({ type: "ping", nonce: 2n })]);
});

test("the handshake times out", () => {
  const c = client(1);
  c.connect(5n);
  assert.deepStrictEqual(c.tick(14n), []);
  assert.deepStrictEqual(c.tick(15n), [{ type: "close", reason: { kind: "handshake_timeout" } }]);
  assert.equal(c.phase(), "disconnected");
});

test("close is terminal and idempotent", () => {
  const c = client(1);
  ready(c);
  assert.deepStrictEqual(c.close(), [{ type: "close", reason: { kind: "by_application" } }]);
  assert.deepStrictEqual(c.close(), []);
  assert.equal(c.phase(), "closed");
  c.disconnected();
  assert.equal(c.phase(), "closed");
});

test("invalid inputs are errors", () => {
  const bad = (patch: Partial<ClientConfig>): unknown => thrown(() => new SyncClient({ ...defaultConfig("a"), ...patch }));
  for (const patch of [
    { agent: "" },
    { agent: "a".repeat(65) },
    { agent: "a\nb" },
    { minVersion: 0 },
    { minVersion: 2, maxVersion: 1 },
    { maxVersion: 65_536 },
    { ackBatch: 0 },
    { pingTimeout: 0n },
  ] satisfies Partial<ClientConfig>[]) {
    assert.ok(bad(patch) instanceof ClientError, JSON.stringify(patch, (_, v: unknown) => (typeof v === "bigint" ? String(v) : v)));
  }
  const c = client(1);
  ready(c);
  const sub = (scope: string, cursor: Uint8Array): unknown => thrown(() => c.subscribe(scope, cursor));
  assert.deepStrictEqual(sub("", new Uint8Array(0)), new ClientError("invalid_scope"));
  assert.deepStrictEqual(sub("a\nb", new Uint8Array(0)), new ClientError("invalid_scope"));
  assert.deepStrictEqual(sub("a", new Uint8Array(65)), new ClientError("invalid_cursor"));
});

test("the client keeps its own copy of what it is given", () => {
  const c = client(1);
  ready(c);
  const start = new Uint8Array([0, 0, 0, 0, 0, 0, 0, 1]);
  const { sub } = c.subscribe("s", start);
  start.fill(9);
  assert.deepStrictEqual(c.resumeCursor(sub), new Uint8Array([0, 0, 0, 0, 0, 0, 0, 1]));
});

test("cursors compare by length first, then by bytes", () => {
  const c = client(1);
  ready(c);
  const { sub } = c.subscribe("s", new Uint8Array(0));
  const out = c.handle(
    {
      type: "events",
      sub,
      events: [
        { cursor: new Uint8Array([0xff]), data: new Uint8Array(0) },
        { cursor: new Uint8Array([0, 0]), data: new Uint8Array(0) }, // longer, so after
        { cursor: new Uint8Array([0xff]), data: new Uint8Array(0) }, // shorter again, dropped
        { cursor: new Uint8Array([0, 1]), data: new Uint8Array(0) },
      ],
    },
    0n,
  );
  assert.equal(out.filter((a) => a.type === "deliver").length, 3);
});

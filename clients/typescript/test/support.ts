// Helpers shared by the tests: the vector file, JSON to value conversion and a seeded PRNG.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { fromHex, type ChangeEvent, type ErrorCode, type Message } from "../src/index.js";
import { ERROR_CODE_NAMES } from "../src/index.js";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type Obj = { [key: string]: Json };

/** The vectors the Rust side wrote: `conformance/vectors.json` at the repository root. */
export function loadVectors(): Obj {
  // Tests run from dist/test, four levels below the repository root.
  const path = resolve(import.meta.dirname, "../../../../conformance/vectors.json");
  return JSON.parse(readFileSync(path, "utf8")) as Obj;
}

export function obj(j: Json | undefined): Obj {
  assert.ok(j !== null && typeof j === "object" && !Array.isArray(j), "expected an object");
  return j;
}

export function arr(j: Json | undefined): Json[] {
  assert.ok(Array.isArray(j), "expected an array");
  return j;
}

export function str(j: Json | undefined): string {
  assert.equal(typeof j, "string");
  return j as string;
}

export function num(j: Json | undefined): number {
  assert.equal(typeof j, "number");
  return j as number;
}

export function big(j: Json | undefined): bigint {
  const s = str(j);
  assert.match(s, /^[0-9]+$/, "expected a decimal string");
  return BigInt(s);
}

/** Hex text, or `{"fill":"ab","len":N}`. */
export function bytesOf(j: Json | undefined): Uint8Array {
  if (typeof j === "string") {
    return fromHex(j);
  }
  const o = obj(j);
  return new Uint8Array(num(o["len"])).fill(fromHex(str(o["fill"]))[0] as number);
}

export function messageFromJson(j: Json | undefined): Message {
  const o = obj(j);
  switch (str(o["type"])) {
    case "hello":
      return {
        type: "hello",
        minVersion: num(o["min_version"]),
        maxVersion: num(o["max_version"]),
        agent: str(o["agent"]),
      };
    case "welcome":
      return {
        type: "welcome",
        version: num(o["version"]),
        maxPayload: num(o["max_payload"]),
        server: str(o["server"]),
      };
    case "subscribe":
      return { type: "subscribe", sub: num(o["sub"]), scope: str(o["scope"]), cursor: bytesOf(o["cursor"]) };
    case "events":
      return {
        type: "events",
        sub: num(o["sub"]),
        events: arr(o["events"]).map((e) => ({
          cursor: bytesOf(obj(e)["cursor"]),
          data: bytesOf(obj(e)["data"]),
        })),
      };
    case "ack":
      return { type: "ack", sub: num(o["sub"]), cursor: bytesOf(o["cursor"]) };
    case "error": {
      const code = str(o["code"]);
      assert.ok((ERROR_CODE_NAMES as readonly string[]).includes(code), `unknown code ${code}`);
      return { type: "error", code: code as ErrorCode, sub: num(o["sub"]), message: str(o["message"]) };
    }
    case "ping":
      return { type: "ping", nonce: big(o["nonce"]) };
    case "pong":
      return { type: "pong", nonce: big(o["nonce"]) };
    default:
      throw new Error(`unknown message type in vectors: ${str(o["type"])}`);
  }
}

export function eventFromJson(j: Json | undefined): ChangeEvent {
  const o = obj(j);
  const cursor = obj(o["cursor"]);
  const hlc = obj(o["hlc"]);
  const turn = o["turn"];
  const op = obj(o["op"]);
  return {
    cursor: { lsn: big(cursor["lsn"]), index: num(cursor["index"]) },
    setLen: num(o["set_len"]),
    changeSet: bytesOf(o["change_set"]),
    hlc: { wallMs: big(hlc["wall_ms"]), counter: num(hlc["counter"]), node: num(hlc["node"]) },
    author: big(o["author"]),
    turn: turn === null ? null : { session: big(obj(turn)["session"]), turn: big(obj(turn)["turn"]) },
    kind: num(o["kind"]),
    key: bytesOf(o["key"]),
    op: str(op["type"]) === "delete" ? { type: "delete" } : { type: "upsert", value: bytesOf(op["value"]) },
  };
}

/** The error an `Error` raised by the codec must match: same kind and same detail as Rust's. */
export function assertErrorMatches(err: unknown, expected: Json | undefined, label: string): void {
  const e = obj(expected);
  assert.ok(err instanceof Error, `${label}: expected an error, got ${String(err)}`);
  const got = err as Error & { kind?: string; detail?: unknown };
  assert.equal(got.kind, str(e["kind"]), `${label}: kind (${got.message})`);
  const want = e["value"] ?? e["field"];
  assert.equal(got.detail, want, `${label}: detail (${got.message})`);
}

/** Runs `fn`, requires it to throw, and returns what it threw. */
export function thrown(fn: () => unknown): unknown {
  try {
    fn();
  } catch (e) {
    return e;
  }
  return assert.fail("expected the call to throw");
}

/** mulberry32: a small seeded PRNG, so a failing case can be replayed from its seed. */
export class Rng {
  private state: number;

  constructor(readonly seed: number) {
    this.state = seed >>> 0;
  }

  /** A float in [0, 1). */
  next(): number {
    this.state = (this.state + 0x6d2b79f5) >>> 0;
    let t = this.state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  }

  /** An integer in [0, n). */
  int(n: number): number {
    return Math.floor(this.next() * n);
  }

  /** An integer in [lo, hi]. */
  range(lo: number, hi: number): number {
    return lo + this.int(hi - lo + 1);
  }

  bool(): boolean {
    return this.next() < 0.5;
  }

  pick<T>(items: readonly T[]): T {
    return items[this.int(items.length)] as T;
  }

  bytes(n: number): Uint8Array {
    const out = new Uint8Array(n);
    for (let i = 0; i < n; i++) {
      out[i] = this.int(256);
    }
    return out;
  }

  /** A random u64, biased to include the edges. */
  u64(): bigint {
    const edges = [0n, 1n, 0xffff_ffff_ffff_ffffn, 1n << 53n, (1n << 53n) + 1n, 0x7fff_ffff_ffff_ffffn];
    if (this.int(4) === 0) {
      return this.pick(edges);
    }
    let v = 0n;
    for (let i = 0; i < 8; i++) {
      v = (v << 8n) | BigInt(this.int(256));
    }
    return v;
  }

  u32(): number {
    const edges = [0, 1, 0xffff_ffff, 0x8000_0000];
    return this.int(4) === 0 ? this.pick(edges) : this.int(0x1_0000_0000);
  }
}

/** The number of random cases per test; override with `LATTICE_TS_CASES`. */
export function cases(fallback: number): number {
  const raw = process.env["LATTICE_TS_CASES"];
  const n = raw === undefined ? NaN : Number.parseInt(raw, 10);
  return Number.isInteger(n) && n > 0 ? n : fallback;
}

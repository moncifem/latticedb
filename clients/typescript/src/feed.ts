// Change feed v1: cursor and event codecs (formats/change-feed-v1.md). A port of
// `lattice_core::feed` with the same bounds, canonical form and order of checks. Every u64 is a
// bigint and the u128 author is a bigint too, so no value is ever rounded.

import {
  Reader,
  U32_MAX,
  U64_MAX,
  U128_MAX,
  Writer,
  compareBytes,
  isBigUint,
  isUint,
} from "./bytes.js";

/** Largest entity key an event may carry, in bytes. */
export const MAX_KEY_LEN = 4096;
/** Largest value an upsert may carry, in bytes. */
export const MAX_VALUE_LEN = 16 * 1024 * 1024;
/** Encoded cursor size in bytes. */
export const CURSOR_LEN = 12;

const VERSION = 1;

/** Why a feed value could not be built or encoded. Mirrors `FeedError`. */
export type FeedErrorKind =
  | "zero_lsn"
  | "index_out_of_range"
  | "key_too_large"
  | "value_too_large"
  | "invalid_value";

export class FeedError extends Error {
  override readonly name = "FeedError";

  constructor(readonly kind: FeedErrorKind) {
    super(kind);
  }
}

/** Why bytes are not a canonical feed value. Mirrors `FeedDecodeError`. */
export type FeedDecodeErrorKind = "truncated" | "version" | "trailing" | "invalid";

export class FeedDecodeError extends Error {
  override readonly name = "FeedDecodeError";
  /** The version byte for `version`, the field name for `invalid`. */
  readonly detail: number | string | undefined;

  constructor(
    readonly kind: FeedDecodeErrorKind,
    detail?: number | string,
  ) {
    super(detail === undefined ? kind : `${kind}: ${String(detail)}`);
    this.detail = detail;
  }
}

/** A position in the feed: ordered by `lsn`, then `index`. */
export interface Cursor {
  readonly lsn: bigint;
  readonly index: number;
}

/** Before every event. */
export const ORIGIN: Cursor = { lsn: 0n, index: 0 };

/** A cursor at event `index` of the change set logged at `lsn`. LSNs start at 1. */
export function newCursor(lsn: bigint, index: number): Cursor {
  if (!isBigUint(lsn, U64_MAX) || !isUint(index, U32_MAX)) {
    throw new FeedError("invalid_value");
  }
  if (lsn === 0n) {
    throw new FeedError("zero_lsn");
  }
  return { lsn, index };
}

/** -1, 0 or 1 by `(lsn, index)`. */
export function compareCursors(a: Cursor, b: Cursor): -1 | 0 | 1 {
  if (a.lsn !== b.lsn) {
    return a.lsn < b.lsn ? -1 : 1;
  }
  if (a.index !== b.index) {
    return a.index < b.index ? -1 : 1;
  }
  return 0;
}

function writeCursor(w: Writer, c: Cursor): void {
  if (!isBigUint(c.lsn, U64_MAX) || !isUint(c.index, U32_MAX)) {
    throw new FeedError("invalid_value");
  }
  w.u64be(c.lsn);
  w.u32be(c.index);
}

/** Encodes as `lsn u64 | index u32`, both big endian: comparing the bytes gives cursor order. */
export function cursorToBytes(c: Cursor): Uint8Array {
  const w = new Writer();
  writeCursor(w, c);
  return w.finish();
}

function need<T>(v: T | null): T {
  if (v === null) {
    throw new FeedDecodeError("truncated");
  }
  return v;
}

function readCursor(r: Reader): Cursor {
  const lsn = need(r.u64be());
  const index = need(r.u32be());
  if (lsn === 0n && index !== 0) {
    throw new FeedDecodeError("invalid", "cursor");
  }
  return { lsn, index };
}

/** Decodes exactly 12 bytes. The origin is the only cursor with LSN 0 and it has index 0. */
export function cursorFromBytes(buf: Uint8Array): Cursor {
  const r = new Reader(buf);
  const c = readCursor(r);
  if (r.remaining !== 0) {
    throw new FeedDecodeError("trailing");
  }
  return c;
}

/** Commit timestamp of a change set. */
export interface Hlc {
  readonly wallMs: bigint;
  readonly counter: number;
  readonly node: number;
}

/** The turn a write belonged to. */
export interface TurnRef {
  readonly session: bigint;
  readonly turn: bigint;
}

/** What happened to the entity. */
export type ChangeOp =
  | { readonly type: "delete" }
  | { readonly type: "upsert"; readonly value: Uint8Array };

/** One entity change of one committed change set, with its commit provenance. */
export interface ChangeEvent {
  readonly cursor: Cursor;
  /** Number of events the change set produced. */
  readonly setLen: number;
  /** The change set id, 16 bytes (big endian ULID). */
  readonly changeSet: Uint8Array;
  readonly hlc: Hlc;
  /** Who made the change, a u128. */
  readonly author: bigint;
  readonly turn: TurnRef | null;
  /** Table or collection of the entity. */
  readonly kind: number;
  readonly key: Uint8Array;
  readonly op: ChangeOp;
}

function checkChange(ev: ChangeEvent): void {
  if (!(ev.key instanceof Uint8Array) || ev.key.length > MAX_KEY_LEN) {
    throw new FeedError("key_too_large");
  }
  if (ev.op.type === "upsert" && ev.op.value.length > MAX_VALUE_LEN) {
    throw new FeedError("value_too_large");
  }
}

/** Encodes to the v1 layout. Throws {@link FeedError}. */
export function encodeEvent(ev: ChangeEvent): Uint8Array {
  if (ev.cursor.lsn === 0n) {
    throw new FeedError("zero_lsn");
  }
  if (!isUint(ev.setLen, U32_MAX) || !isUint(ev.cursor.index, U32_MAX)) {
    throw new FeedError("invalid_value");
  }
  if (ev.cursor.index >= ev.setLen) {
    throw new FeedError("index_out_of_range");
  }
  checkChange(ev);
  if (
    !(ev.changeSet instanceof Uint8Array) ||
    ev.changeSet.length !== 16 ||
    !isBigUint(ev.hlc.wallMs, U64_MAX) ||
    !isUint(ev.hlc.counter, U32_MAX) ||
    !isUint(ev.hlc.node, U32_MAX) ||
    !isBigUint(ev.author, U128_MAX) ||
    !isUint(ev.kind, U32_MAX)
  ) {
    throw new FeedError("invalid_value");
  }
  const w = new Writer();
  w.u8(VERSION);
  writeCursor(w, ev.cursor);
  w.u32le(ev.setLen);
  w.bytes(ev.changeSet);
  w.u64le(ev.hlc.wallMs);
  w.u32le(ev.hlc.counter);
  w.u32le(ev.hlc.node);
  w.u128be(ev.author);
  if (ev.turn === null) {
    w.u8(0);
  } else {
    if (!isBigUint(ev.turn.session, U64_MAX) || !isBigUint(ev.turn.turn, U64_MAX)) {
      throw new FeedError("invalid_value");
    }
    w.u8(1);
    w.u64le(ev.turn.session);
    w.u64le(ev.turn.turn);
  }
  w.u32le(ev.kind);
  w.u16le(ev.key.length);
  w.bytes(ev.key);
  if (ev.op.type === "delete") {
    w.u8(0);
  } else {
    w.u8(1);
    w.u32le(ev.op.value.length);
    w.bytes(ev.op.value);
  }
  return w.finish();
}

/** Decodes the v1 layout. Only the bytes {@link encodeEvent} produces are accepted. */
export function decodeEvent(buf: Uint8Array): ChangeEvent {
  const r = new Reader(buf);
  const v = need(r.u8());
  if (v !== VERSION) {
    throw new FeedDecodeError("version", v);
  }
  const cursor = readCursor(r);
  if (cursor.lsn === 0n) {
    throw new FeedDecodeError("invalid", "event lsn");
  }
  const setLen = need(r.u32le());
  if (cursor.index >= setLen) {
    throw new FeedDecodeError("invalid", "event index");
  }
  const changeSet = need(r.take(16));
  const hlc: Hlc = {
    wallMs: need(r.u64le()),
    counter: need(r.u32le()),
    node: need(r.u32le()),
  };
  const author = need(r.u128be());
  let turn: TurnRef | null;
  switch (need(r.u8())) {
    case 0:
      turn = null;
      break;
    case 1:
      turn = { session: need(r.u64le()), turn: need(r.u64le()) };
      break;
    default:
      throw new FeedDecodeError("invalid", "turn flag");
  }
  const kind = need(r.u32le());
  const keyLen = need(r.u16le());
  if (keyLen > MAX_KEY_LEN) {
    throw new FeedDecodeError("invalid", "key length");
  }
  const key = need(r.take(keyLen));
  let op: ChangeOp;
  switch (need(r.u8())) {
    case 0:
      op = { type: "delete" };
      break;
    case 1: {
      const len = need(r.u32le());
      if (len > MAX_VALUE_LEN) {
        throw new FeedDecodeError("invalid", "value length");
      }
      // Checked against the bytes that remain before anything is allocated.
      op = { type: "upsert", value: need(r.take(len)) };
      break;
    }
    default:
      throw new FeedDecodeError("invalid", "operation");
  }
  if (r.remaining !== 0) {
    throw new FeedDecodeError("trailing");
  }
  return { cursor, setLen, changeSet, hlc, author, turn, kind, key, op };
}

/** Compares two encoded cursors as unsigned strings; equals {@link compareCursors} on decoded ones. */
export function compareEncodedCursors(a: Uint8Array, b: Uint8Array): -1 | 0 | 1 {
  return compareBytes(a, b);
}

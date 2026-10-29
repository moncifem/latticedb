// Sync protocol v1 codec (formats/sync-protocol-v1.md). A port of `lattice_api::sync`: the same
// bounds, the same canonical encodings, the same order of checks, so the same input fails with the
// same error. Pure functions over bytes: no I/O, no clock, no session state.

import { Reader, U32_MAX, U64_MAX, Writer, isBigUint, isUint } from "./bytes.js";

/** The lowest protocol version this implementation speaks. */
export const VERSION_MIN = 1;
/** The highest protocol version this implementation speaks. */
export const VERSION_MAX = 1;
/** Bytes before the payload: a type byte and a four-byte payload length. */
export const HEADER_LEN = 5;
/** The largest payload the codec accepts or produces, in bytes. */
export const MAX_PAYLOAD = 1 << 20;
/** The smallest payload limit a `Welcome` may announce. */
export const MIN_NEGOTIATED_PAYLOAD = 1024;
/** The longest cursor, in bytes. */
export const MAX_CURSOR_LEN = 64;
/** The longest subscription scope, in bytes of UTF-8. */
export const MAX_SCOPE_LEN = 512;
/** The longest agent or server name, in bytes of UTF-8. */
export const MAX_NAME_LEN = 64;
/** The longest error message, in bytes of UTF-8. */
export const MAX_ERROR_MESSAGE_LEN = 256;
/** The most events in one `Events` frame. */
export const MAX_EVENTS_PER_FRAME = 256;

const TYPE_HELLO = 0x01;
const TYPE_WELCOME = 0x02;
const TYPE_SUBSCRIBE = 0x03;
const TYPE_EVENTS = 0x04;
const TYPE_ACK = 0x05;
const TYPE_ERROR = 0x06;
const TYPE_PING = 0x07;
const TYPE_PONG = 0x08;

/** Why a frame could not be decoded or encoded. Mirrors `lattice_api::sync::SyncError`. */
export type SyncErrorKind =
  | "bad_length"
  | "unknown_type"
  | "truncated"
  | "trailing"
  | "not_utf8"
  | "too_large"
  | "invalid_field";

export class SyncError extends Error {
  override readonly name = "SyncError";
  /**
   * Extra detail: the offending length for `bad_length`, the type byte for `unknown_type`, the
   * field name for `invalid_field`. Absent for the other kinds.
   */
  readonly detail: number | string | undefined;

  constructor(
    readonly kind: SyncErrorKind,
    detail?: number | string,
  ) {
    super(detail === undefined ? kind : `${kind}: ${String(detail)}`);
    this.detail = detail;
  }
}

/** The names of the error codes in an `Error` message, by wire number 1 to 7. */
export const ERROR_CODE_NAMES = [
  "unsupported_version",
  "malformed",
  "unknown_subscription",
  "invalid_scope",
  "invalid_cursor",
  "too_many_subscriptions",
  "internal",
] as const;

export type ErrorCode = (typeof ERROR_CODE_NAMES)[number];

/** The number on the wire for an error code. */
export function errorCodeNumber(code: ErrorCode): number {
  const i = ERROR_CODE_NAMES.indexOf(code);
  if (i < 0) {
    throw new SyncError("invalid_field", "error code");
  }
  return i + 1;
}

/** The error code for a number on the wire, or `undefined` if the protocol does not define it. */
export function errorCodeFromNumber(n: number): ErrorCode | undefined {
  return ERROR_CODE_NAMES[n - 1];
}

/** One change delivered to a subscription. */
export interface SyncEvent {
  /** The position after this event, 1 to 64 opaque bytes. */
  readonly cursor: Uint8Array;
  /** The event body, opaque to this codec. */
  readonly data: Uint8Array;
}

/** A sync protocol message. */
export type Message =
  | {
      readonly type: "hello";
      readonly minVersion: number;
      readonly maxVersion: number;
      readonly agent: string;
    }
  | {
      readonly type: "welcome";
      readonly version: number;
      readonly maxPayload: number;
      readonly server: string;
    }
  | {
      readonly type: "subscribe";
      readonly sub: number;
      readonly scope: string;
      readonly cursor: Uint8Array;
    }
  | {
      readonly type: "events";
      readonly sub: number;
      readonly events: readonly SyncEvent[];
    }
  | { readonly type: "ack"; readonly sub: number; readonly cursor: Uint8Array }
  | {
      readonly type: "error";
      readonly code: ErrorCode;
      readonly sub: number;
      readonly message: string;
    }
  | { readonly type: "ping"; readonly nonce: bigint }
  | { readonly type: "pong"; readonly nonce: bigint };

/** The result of {@link negotiate}. */
export type Negotiation =
  | { readonly ok: true; readonly version: number }
  | { readonly ok: false; readonly error: "malformed" | "unsupported_version" };

/**
 * Picks the version for a connection: the highest one both sides speak. The client offers
 * `min..=max`; this implementation speaks {@link VERSION_MIN}..={@link VERSION_MAX}.
 */
export function negotiate(min: number, max: number): Negotiation {
  if (!isUint(min, 0xffff) || !isUint(max, 0xffff) || min < VERSION_MIN || min > max) {
    return { ok: false, error: "malformed" };
  }
  if (min > VERSION_MAX || max < VERSION_MIN) {
    return { ok: false, error: "unsupported_version" };
  }
  return { ok: true, version: Math.min(max, VERSION_MAX) };
}

const utf8Encoder = new TextEncoder();
const utf8Decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });

// Unicode category Cc, the same set as Rust's `char::is_control`.
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;

function hasLoneSurrogate(s: string): boolean {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdbff) {
      const next = s.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        i++;
        continue;
      }
      return true;
    }
    if (c >= 0xdc00 && c <= 0xdfff) {
      return true;
    }
  }
  return false;
}

function checkText(byteLen: number, s: string, min: number, max: number, what: string): void {
  if (byteLen < min || byteLen > max || CONTROL.test(s)) {
    throw new SyncError("invalid_field", what);
  }
}

function checkVersions(min: number, max: number): void {
  if (!isUint(min, 0xffff) || !isUint(max, 0xffff) || min < VERSION_MIN || min > max) {
    throw new SyncError("invalid_field", "version range");
  }
}

// A Welcome names the one version this implementation speaks; an unknown version is refused
// rather than guessed at.
function checkWelcome(version: number, maxPayload: number): void {
  if (!isUint(version, 0xffff) || version < VERSION_MIN || version > VERSION_MAX) {
    throw new SyncError("invalid_field", "version");
  }
  if (!isUint(maxPayload, U32_MAX) || maxPayload < MIN_NEGOTIATED_PAYLOAD || maxPayload > MAX_PAYLOAD) {
    throw new SyncError("invalid_field", "max payload");
  }
}

function checkSub(sub: number): void {
  if (!isUint(sub, U32_MAX) || sub === 0) {
    throw new SyncError("invalid_field", "subscription id");
  }
}

function putText(w: Writer, s: string, min: number, max: number, what: string): void {
  if (typeof s !== "string" || hasLoneSurrogate(s)) {
    throw new SyncError("not_utf8");
  }
  const bytes = utf8Encoder.encode(s);
  checkText(bytes.length, s, min, max, what);
  w.u16be(bytes.length);
  w.bytes(bytes);
}

function putCursor(w: Writer, c: Uint8Array, min: number): void {
  if (!(c instanceof Uint8Array) || c.length < min || c.length > MAX_CURSOR_LEN) {
    throw new SyncError("invalid_field", "cursor");
  }
  w.u16be(c.length);
  w.bytes(c);
}

/**
 * The frame for `msg`. Refuses any message the decoder would refuse, so a message that encodes
 * always decodes to itself. Throws {@link SyncError}.
 */
export function encode(msg: Message): Uint8Array {
  const w = new Writer();
  let ty: number;
  switch (msg.type) {
    case "hello":
      checkVersions(msg.minVersion, msg.maxVersion);
      w.u16be(msg.minVersion);
      w.u16be(msg.maxVersion);
      putText(w, msg.agent, 1, MAX_NAME_LEN, "agent");
      ty = TYPE_HELLO;
      break;
    case "welcome":
      checkWelcome(msg.version, msg.maxPayload);
      w.u16be(msg.version);
      w.u32be(msg.maxPayload);
      putText(w, msg.server, 1, MAX_NAME_LEN, "server");
      ty = TYPE_WELCOME;
      break;
    case "subscribe":
      checkSub(msg.sub);
      w.u32be(msg.sub);
      putText(w, msg.scope, 1, MAX_SCOPE_LEN, "scope");
      putCursor(w, msg.cursor, 0);
      ty = TYPE_SUBSCRIBE;
      break;
    case "events":
      checkSub(msg.sub);
      if (msg.events.length === 0 || msg.events.length > MAX_EVENTS_PER_FRAME) {
        throw new SyncError("invalid_field", "event count");
      }
      w.u32be(msg.sub);
      w.u16be(msg.events.length);
      for (const e of msg.events) {
        putCursor(w, e.cursor, 1);
        if (!(e.data instanceof Uint8Array)) {
          throw new SyncError("invalid_field", "data");
        }
        if (w.length + 4 + e.data.length > MAX_PAYLOAD) {
          throw new SyncError("too_large");
        }
        w.u32be(e.data.length);
        w.bytes(e.data);
      }
      ty = TYPE_EVENTS;
      break;
    case "ack":
      checkSub(msg.sub);
      w.u32be(msg.sub);
      putCursor(w, msg.cursor, 1);
      ty = TYPE_ACK;
      break;
    case "error":
      w.u16be(errorCodeNumber(msg.code));
      if (!isUint(msg.sub, U32_MAX)) {
        throw new SyncError("invalid_field", "subscription id");
      }
      w.u32be(msg.sub);
      putText(w, msg.message, 0, MAX_ERROR_MESSAGE_LEN, "message");
      ty = TYPE_ERROR;
      break;
    case "ping":
    case "pong":
      if (!isBigUint(msg.nonce, U64_MAX)) {
        throw new SyncError("invalid_field", "nonce");
      }
      w.u64be(msg.nonce);
      ty = msg.type === "ping" ? TYPE_PING : TYPE_PONG;
      break;
    default:
      throw new SyncError("unknown_type");
  }
  if (w.length > MAX_PAYLOAD) {
    throw new SyncError("too_large");
  }
  const payload = w.finish();
  const out = new Writer();
  out.u8(ty);
  out.u32be(payload.length);
  out.bytes(payload);
  return out.finish();
}

/** A decoded frame and the number of bytes it used. */
export interface Decoded {
  readonly message: Message;
  readonly length: number;
}

/**
 * Decodes one frame from the front of `buf` with the default payload limit, {@link MAX_PAYLOAD}.
 * Returns `null` when `buf` holds only part of a frame. Throws {@link SyncError}.
 */
export function decode(buf: Uint8Array): Decoded | null {
  return decodeWithLimit(buf, MAX_PAYLOAD);
}

/**
 * Like {@link decode}, with a payload limit negotiated lower than {@link MAX_PAYLOAD} (a higher
 * limit is treated as {@link MAX_PAYLOAD}). The type and the length are judged as soon as the five
 * header bytes are present, so a hostile peer cannot make the reader wait for a payload it will
 * refuse.
 */
export function decodeWithLimit(buf: Uint8Array, limit: number): Decoded | null {
  if (buf.length === 0) {
    return null;
  }
  const ty = buf[0] as number;
  if (ty < TYPE_HELLO || ty > TYPE_PONG) {
    throw new SyncError("unknown_type", ty);
  }
  if (buf.length < HEADER_LEN) {
    return null;
  }
  const len = new DataView(buf.buffer, buf.byteOffset, buf.byteLength).getUint32(1, false);
  if (len > Math.min(limit, MAX_PAYLOAD)) {
    throw new SyncError("bad_length", len);
  }
  const end = HEADER_LEN + len;
  if (buf.length < end) {
    return null;
  }
  return { message: decodePayload(ty, buf.subarray(HEADER_LEN, end)), length: end };
}

function need<T>(v: T | null): T {
  if (v === null) {
    throw new SyncError("truncated");
  }
  return v;
}

function readText(r: Reader, min: number, max: number, what: string): string {
  const n = need(r.u16be());
  if (n < min || n > max) {
    throw new SyncError("invalid_field", what);
  }
  const bytes = need(r.take(n));
  let s: string;
  try {
    s = utf8Decoder.decode(bytes);
  } catch {
    throw new SyncError("not_utf8");
  }
  checkText(n, s, min, max, what);
  return s;
}

function readCursor(r: Reader, min: number): Uint8Array {
  const n = need(r.u16be());
  if (n < min || n > MAX_CURSOR_LEN) {
    throw new SyncError("invalid_field", "cursor");
  }
  return need(r.take(n));
}

function decodePayload(ty: number, payload: Uint8Array): Message {
  const r = new Reader(payload);
  let msg: Message;
  switch (ty) {
    case TYPE_HELLO: {
      const minVersion = need(r.u16be());
      const maxVersion = need(r.u16be());
      checkVersions(minVersion, maxVersion);
      msg = { type: "hello", minVersion, maxVersion, agent: readText(r, 1, MAX_NAME_LEN, "agent") };
      break;
    }
    case TYPE_WELCOME: {
      const version = need(r.u16be());
      const maxPayload = need(r.u32be());
      checkWelcome(version, maxPayload);
      msg = { type: "welcome", version, maxPayload, server: readText(r, 1, MAX_NAME_LEN, "server") };
      break;
    }
    case TYPE_SUBSCRIBE: {
      const sub = need(r.u32be());
      checkSub(sub);
      const scope = readText(r, 1, MAX_SCOPE_LEN, "scope");
      msg = { type: "subscribe", sub, scope, cursor: readCursor(r, 0) };
      break;
    }
    case TYPE_EVENTS: {
      const sub = need(r.u32be());
      checkSub(sub);
      const count = need(r.u16be());
      if (count === 0 || count > MAX_EVENTS_PER_FRAME) {
        throw new SyncError("invalid_field", "event count");
      }
      const events: SyncEvent[] = [];
      for (let i = 0; i < count; i++) {
        const cursor = readCursor(r, 1);
        const n = need(r.u32be());
        events.push({ cursor, data: need(r.take(n)) });
      }
      msg = { type: "events", sub, events };
      break;
    }
    case TYPE_ACK: {
      const sub = need(r.u32be());
      checkSub(sub);
      msg = { type: "ack", sub, cursor: readCursor(r, 1) };
      break;
    }
    case TYPE_ERROR: {
      const code = errorCodeFromNumber(need(r.u16be()));
      if (code === undefined) {
        throw new SyncError("invalid_field", "error code");
      }
      const sub = need(r.u32be());
      msg = { type: "error", code, sub, message: readText(r, 0, MAX_ERROR_MESSAGE_LEN, "message") };
      break;
    }
    case TYPE_PING:
      msg = { type: "ping", nonce: need(r.u64be()) };
      break;
    case TYPE_PONG:
      msg = { type: "pong", nonce: need(r.u64be()) };
      break;
    default:
      throw new SyncError("unknown_type", ty);
  }
  if (r.remaining !== 0) {
    throw new SyncError("trailing");
  }
  return msg;
}

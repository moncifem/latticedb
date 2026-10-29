// Sans-I/O client for the sync protocol v1: the same state machine as
// `lattice_sdk::client::SyncClient`. The caller owns the socket, the framing and the clock. It
// decodes frames with `decode`, passes them to `handle` with a logical time, writes every `send`
// action it gets back and calls `tick` now and then.
//
// Delivery is at least once. After a reconnect the client resubscribes with the last cursor it
// acknowledged, so the server may replay events the application already saw. The client drops every
// event whose cursor is not strictly greater than the last one it delivered (by length first, then
// byte by byte), so the application sees each event once and in order. Times and nonces are u64 in
// the Rust client and bigint here.

import { U32_MAX, U64_MAX, compareBytes, isBigUint } from "./bytes.js";
import {
  MAX_CURSOR_LEN,
  MAX_ERROR_MESSAGE_LEN,
  MAX_NAME_LEN,
  MAX_SCOPE_LEN,
  type ErrorCode,
  type Message,
  type SyncEvent,
} from "./sync.js";

/** Lowest protocol version this client can speak. */
export const SUPPORTED_MIN_VERSION = 1;
/** Highest protocol version this client can speak. */
export const SUPPORTED_MAX_VERSION = 1;

/** Connection phase. */
export type Phase = "disconnected" | "hello_sent" | "ready" | "subscribed" | "closed";

/** Why a connection ended. */
export type CloseReason =
  | { readonly kind: "incompatible" }
  | { readonly kind: "server_error"; readonly code: ErrorCode; readonly message: string }
  | { readonly kind: "protocol_violation"; readonly rule: string }
  | { readonly kind: "ping_timeout" }
  | { readonly kind: "handshake_timeout" }
  | { readonly kind: "by_application" };

/** What the caller must do after an input. */
export type Action =
  | { readonly type: "send"; readonly frame: Message }
  | { readonly type: "ready"; readonly version: number; readonly maxPayload: number }
  | {
      readonly type: "deliver";
      readonly sub: number;
      readonly cursor: Uint8Array;
      readonly data: Uint8Array;
    }
  | {
      readonly type: "subscription_failed";
      readonly sub: number;
      readonly code: ErrorCode;
      readonly message: string;
    }
  | { readonly type: "close"; readonly reason: CloseReason };

export type ClientErrorKind =
  | "invalid_config"
  | "wrong_phase"
  | "invalid_scope"
  | "invalid_cursor"
  | "unknown_subscription"
  | "ack_beyond_delivered"
  | "subscription_ids_exhausted";

/** A call that does not fit the client's state or limits. */
export class ClientError extends Error {
  override readonly name = "ClientError";

  /**
   * @param detail the rejected setting for `invalid_config`, the phase for `wrong_phase`, the
   * subscription id for `unknown_subscription`.
   */
  constructor(
    readonly kind: ClientErrorKind,
    readonly detail?: string | number,
  ) {
    super(detail === undefined ? kind : `${kind}: ${String(detail)}`);
  }
}

/** Client settings. Times are in the caller's logical ticks. */
export interface ClientConfig {
  /** Lowest version offered in `Hello`, at least 1. */
  minVersion: number;
  /** Highest version offered in `Hello`. */
  maxVersion: number;
  /** Agent name, 1 to 64 bytes. */
  agent: string;
  /** Idle ticks after which the client sends a `Ping`. */
  pingInterval: bigint;
  /** Ticks without an answer after which the connection is dropped, at least 1. */
  pingTimeout: bigint;
  /** Number of `ack` calls merged into one wire `Ack`, at least 1. */
  ackBatch: number;
}

/** Defaults: the supported version range, 30 tick interval, 10 tick timeout, no batching. */
export function defaultConfig(agent: string): ClientConfig {
  return {
    minVersion: SUPPORTED_MIN_VERSION,
    maxVersion: SUPPORTED_MAX_VERSION,
    agent,
    pingInterval: 30n,
    pingTimeout: 10n,
    ackBatch: 1,
  };
}

const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;
const encoder = new TextEncoder();

function validate(cfg: ClientConfig): void {
  const u16 = (n: number): boolean => Number.isInteger(n) && n >= 0 && n <= 0xffff;
  if (!u16(cfg.minVersion) || !u16(cfg.maxVersion) || cfg.minVersion === 0 || cfg.minVersion > cfg.maxVersion) {
    throw new ClientError("invalid_config", "version range");
  }
  const nameLen = encoder.encode(cfg.agent).length;
  if (nameLen === 0 || nameLen > MAX_NAME_LEN || CONTROL.test(cfg.agent)) {
    throw new ClientError("invalid_config", "agent name");
  }
  if (!Number.isInteger(cfg.ackBatch) || cfg.ackBatch < 1 || cfg.ackBatch > U32_MAX) {
    throw new ClientError("invalid_config", "ack batch");
  }
  if (!isBigUint(cfg.pingInterval, U64_MAX)) {
    throw new ClientError("invalid_config", "ping interval");
  }
  if (!isBigUint(cfg.pingTimeout, U64_MAX) || cfg.pingTimeout === 0n) {
    throw new ClientError("invalid_config", "ping timeout");
  }
}

interface Sub {
  scope: string;
  /** Last cursor handed to the application (or the starting cursor). */
  delivered: Uint8Array;
  /** Last cursor sent in an `Ack` (or the starting cursor); the resume point. */
  acked: Uint8Array;
  /** Highest cursor the application acknowledged but that is not yet on the wire. */
  processed: Uint8Array | null;
  /** `ack` calls merged into `processed`. */
  pendingAcks: number;
}

/** `a` is strictly after `b`: longer, or equal length and bytewise greater. */
export function cursorAfter(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) {
    return a.length > b.length;
  }
  return compareBytes(a, b) > 0;
}

/** At most 256 bytes of `text`, cut on a character boundary. */
function clip(text: string): string {
  let out = "";
  let bytes = 0;
  for (const ch of text) {
    const n = encoder.encode(ch).length;
    if (bytes + n > MAX_ERROR_MESSAGE_LEN) {
      break;
    }
    out += ch;
    bytes += n;
  }
  return out;
}

const satSub = (a: bigint, b: bigint): bigint => (a > b ? a - b : 0n);

/**
 * The sync client state machine. Every method that the Rust client reports with `Err` throws a
 * {@link ClientError} here and leaves the client unchanged.
 */
export class SyncClient {
  private readonly cfg: ClientConfig;
  private currentPhase: Phase = "disconnected";
  private lastRx = 0n;
  private ping: { nonce: bigint; sent: bigint } | null = null;
  private nextNonce = 1n;
  // Subscription ids only grow and a removal never reorders, so insertion order is id order, the
  // order in which the Rust client (a BTreeMap) resubscribes.
  private readonly subs = new Map<number, Sub>();
  private nextSub = 1;
  private negotiated: number | null = null;

  /** Builds a client; refuses a configuration that would produce an invalid `Hello`. */
  constructor(cfg: ClientConfig) {
    validate(cfg);
    this.cfg = { ...cfg };
  }

  phase(): Phase {
    return this.currentPhase;
  }

  /** The negotiated version, once the handshake finished on the current connection. */
  version(): number | null {
    return this.negotiated;
  }

  /** Ids of the live subscriptions, ascending. */
  subscriptions(): number[] {
    return [...this.subs.keys()];
  }

  /** The cursor a reconnect would resume `sub` from (the last acknowledged one). */
  resumeCursor(sub: number): Uint8Array | null {
    return this.subs.get(sub)?.acked.slice() ?? null;
  }

  /** Starts the first connection: returns the `Hello` to send. */
  connect(now: bigint): Action[] {
    if (this.currentPhase !== "disconnected") {
      throw new ClientError("wrong_phase", this.currentPhase);
    }
    return this.begin(now);
  }

  /** Starts a new connection after the old one ended; subscriptions resume on `Welcome`. */
  reconnect(now: bigint): Action[] {
    return this.connect(now);
  }

  /** The transport dropped without a protocol-level reason. Subscriptions are kept. */
  disconnected(): void {
    if (this.currentPhase !== "closed") {
      this.currentPhase = "disconnected";
      this.ping = null;
      this.negotiated = null;
    }
  }

  /** The application is done. Terminal. */
  close(): Action[] {
    if (this.currentPhase === "closed") {
      return [];
    }
    this.currentPhase = "closed";
    return [{ type: "close", reason: { kind: "by_application" } }];
  }

  private begin(now: bigint): Action[] {
    this.currentPhase = "hello_sent";
    this.lastRx = now;
    this.ping = null;
    this.negotiated = null;
    return [
      {
        type: "send",
        frame: {
          type: "hello",
          minVersion: this.cfg.minVersion,
          maxVersion: this.cfg.maxVersion,
          agent: this.cfg.agent,
        },
      },
    ];
  }

  /**
   * Starts a subscription. `cursor` is empty to start from the beginning. Allowed once the
   * handshake finished.
   */
  subscribe(scope: string, cursor: Uint8Array): { sub: number; actions: Action[] } {
    if (this.currentPhase !== "ready" && this.currentPhase !== "subscribed") {
      throw new ClientError("wrong_phase", this.currentPhase);
    }
    const scopeLen = encoder.encode(scope).length;
    if (scopeLen === 0 || scopeLen > MAX_SCOPE_LEN || CONTROL.test(scope)) {
      throw new ClientError("invalid_scope");
    }
    if (cursor.length > MAX_CURSOR_LEN) {
      throw new ClientError("invalid_cursor");
    }
    const sub = this.nextSub;
    if (sub === U32_MAX) {
      throw new ClientError("subscription_ids_exhausted");
    }
    this.nextSub = sub + 1;
    this.subs.set(sub, {
      scope,
      delivered: cursor.slice(),
      acked: cursor.slice(),
      processed: null,
      pendingAcks: 0,
    });
    this.currentPhase = "subscribed";
    return {
      sub,
      actions: [{ type: "send", frame: { type: "subscribe", sub, scope, cursor: cursor.slice() } }],
    };
  }

  /**
   * The application finished processing everything up to `cursor` on `sub`. Calls are batched: a
   * wire `Ack` is produced every `ackBatch` calls, or by {@link SyncClient.flushAcks}.
   */
  ack(sub: number, cursor: Uint8Array): Action[] {
    const s = this.subs.get(sub);
    if (s === undefined) {
      throw new ClientError("unknown_subscription", sub);
    }
    if (cursor.length === 0 || cursor.length > MAX_CURSOR_LEN) {
      throw new ClientError("invalid_cursor");
    }
    if (cursorAfter(cursor, s.delivered)) {
      throw new ClientError("ack_beyond_delivered");
    }
    const newest = s.processed !== null ? cursorAfter(cursor, s.processed) : cursorAfter(cursor, s.acked);
    if (newest) {
      s.processed = cursor.slice();
    }
    s.pendingAcks = Math.min(s.pendingAcks + 1, U32_MAX);
    if (s.pendingAcks >= this.cfg.ackBatch) {
      return this.takeAck(sub, s);
    }
    return [];
  }

  /** Sends the pending acknowledgement of every subscription now. */
  flushAcks(): Action[] {
    const out: Action[] = [];
    for (const [id, s] of this.subs) {
      out.push(...this.takeAck(id, s));
    }
    return out;
  }

  private takeAck(id: number, s: Sub): Action[] {
    s.pendingAcks = 0;
    const cursor = s.processed;
    s.processed = null;
    if (cursor === null) {
      return [];
    }
    // The acknowledgement counts as given even if the connection is down: the resume point moves,
    // and the server learns it from the next Subscribe.
    s.acked = cursor.slice();
    if (this.currentPhase === "ready" || this.currentPhase === "subscribed") {
      return [{ type: "send", frame: { type: "ack", sub: id, cursor } }];
    }
    return [];
  }

  /**
   * Advances the logical clock: sends a `Ping` after `pingInterval` idle ticks and drops the
   * connection when an answer is overdue by `pingTimeout`.
   */
  tick(now: bigint): Action[] {
    const idle = satSub(now, this.lastRx);
    switch (this.currentPhase) {
      case "disconnected":
      case "closed":
        return [];
      case "hello_sent":
        return idle >= this.cfg.pingTimeout ? this.dropConnection({ kind: "handshake_timeout" }) : [];
      case "ready":
      case "subscribed": {
        if (this.ping !== null) {
          return satSub(now, this.ping.sent) >= this.cfg.pingTimeout
            ? this.dropConnection({ kind: "ping_timeout" })
            : [];
        }
        if (idle >= this.cfg.pingInterval) {
          const nonce = this.nextNonce;
          this.nextNonce = (nonce + 1n) & U64_MAX;
          this.ping = { nonce, sent: now };
          return [{ type: "send", frame: { type: "ping", nonce } }];
        }
        return [];
      }
    }
  }

  private dropConnection(reason: CloseReason): Action[] {
    this.currentPhase = "disconnected";
    this.ping = null;
    this.negotiated = null;
    return [{ type: "close", reason }];
  }

  private violation(rule: string): Action[] {
    return [
      { type: "send", frame: { type: "error", code: "malformed", sub: 0, message: clip(rule) } },
      ...this.dropConnection({ kind: "protocol_violation", rule }),
    ];
  }

  /** Feeds one decoded frame from the server. */
  handle(frame: Message, now: bigint): Action[] {
    if (this.currentPhase === "disconnected" || this.currentPhase === "closed") {
      throw new ClientError("wrong_phase", this.currentPhase);
    }
    this.lastRx = now;
    switch (frame.type) {
      case "welcome":
        return this.onWelcome(frame.version, frame.maxPayload);
      case "events":
        if (this.currentPhase === "hello_sent") {
          return this.violation("events before welcome");
        }
        return this.onEvents(frame.sub, frame.events);
      case "error":
        return this.onError(frame.code, frame.sub, frame.message);
      case "ping":
        return [{ type: "send", frame: { type: "pong", nonce: frame.nonce } }];
      case "pong":
        if (this.ping !== null && this.ping.nonce === frame.nonce) {
          this.ping = null;
        }
        return [];
      case "hello":
      case "subscribe":
      case "ack":
        return this.violation("client-to-server frame received");
    }
  }

  private onWelcome(version: number, maxPayload: number): Action[] {
    if (this.currentPhase !== "hello_sent") {
      return this.violation("welcome outside handshake");
    }
    if (version < this.cfg.minVersion || version > this.cfg.maxVersion) {
      this.currentPhase = "closed";
      this.ping = null;
      return [{ type: "close", reason: { kind: "incompatible" } }];
    }
    this.negotiated = version;
    const out: Action[] = [{ type: "ready", version, maxPayload }];
    // Resume: every live subscription restarts from its last acknowledged cursor. Events the
    // application already saw are filtered out by `onEvents`.
    for (const [id, s] of this.subs) {
      out.push({
        type: "send",
        frame: { type: "subscribe", sub: id, scope: s.scope, cursor: s.acked.slice() },
      });
    }
    this.currentPhase = this.subs.size === 0 ? "ready" : "subscribed";
    return out;
  }

  private onEvents(sub: number, events: readonly SyncEvent[]): Action[] {
    const s = this.subs.get(sub);
    if (s === undefined) {
      // A late frame for a subscription that failed or never existed: nothing to deliver.
      return [];
    }
    const out: Action[] = [];
    for (const ev of events) {
      if (ev.cursor.length > MAX_CURSOR_LEN || !cursorAfter(ev.cursor, s.delivered)) {
        continue;
      }
      s.delivered = ev.cursor.slice();
      out.push({ type: "deliver", sub, cursor: ev.cursor.slice(), data: ev.data.slice() });
    }
    return out;
  }

  private onError(code: ErrorCode, sub: number, message: string): Action[] {
    if (sub !== 0) {
      if (this.subs.delete(sub)) {
        if (this.subs.size === 0 && this.currentPhase === "subscribed") {
          this.currentPhase = "ready";
        }
        return [{ type: "subscription_failed", sub, code, message }];
      }
      return [];
    }
    if (code === "unsupported_version") {
      this.currentPhase = "closed";
      this.ping = null;
      this.negotiated = null;
      return [{ type: "close", reason: { kind: "incompatible" } }];
    }
    return this.dropConnection({ kind: "server_error", code, message });
  }
}

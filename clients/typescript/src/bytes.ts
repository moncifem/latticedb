// Byte helpers shared by the codecs. Nothing here depends on Node.

const HEX = "0123456789abcdef";

/** Lowercase hexadecimal, two digits per byte, no separators. */
export function toHex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) {
    out += HEX.charAt(b >> 4) + HEX.charAt(b & 15);
  }
  return out;
}

/** Parses the output of {@link toHex}. Throws a `RangeError` on odd length or a non-hex digit. */
export function fromHex(text: string): Uint8Array {
  if (text.length % 2 !== 0) {
    throw new RangeError("hex text has an odd length");
  }
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i++) {
    const pair = text.slice(2 * i, 2 * i + 2);
    if (!/^[0-9a-fA-F]{2}$/.test(pair)) {
      throw new RangeError("hex text holds a non-hex digit");
    }
    out[i] = Number.parseInt(pair, 16);
  }
  return out;
}

/** Byte-wise equality. */
export function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) {
    return false;
  }
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) {
      return false;
    }
  }
  return true;
}

/** Unsigned lexicographic comparison, shorter prefix first. Returns -1, 0 or 1. */
export function compareBytes(a: Uint8Array, b: Uint8Array): -1 | 0 | 1 {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    const x = a[i] as number;
    const y = b[i] as number;
    if (x !== y) {
      return x < y ? -1 : 1;
    }
  }
  if (a.length === b.length) {
    return 0;
  }
  return a.length < b.length ? -1 : 1;
}

export const U16_MAX = 0xffff;
export const U32_MAX = 0xffff_ffff;
export const U64_MAX = 0xffff_ffff_ffff_ffffn;
export const U128_MAX = (1n << 128n) - 1n;

/** True for an integer `n` with `0 <= n <= max`. */
export function isUint(n: unknown, max: number): n is number {
  return typeof n === "number" && Number.isInteger(n) && n >= 0 && n <= max;
}

/** True for a bigint `n` with `0 <= n <= max`. */
export function isBigUint(n: unknown, max: bigint): n is bigint {
  return typeof n === "bigint" && n >= 0n && n <= max;
}

/** Growable output buffer. */
export class Writer {
  private buf = new Uint8Array(64);
  private len = 0;

  get length(): number {
    return this.len;
  }

  private reserve(extra: number): void {
    const need = this.len + extra;
    if (need <= this.buf.length) {
      return;
    }
    let cap = this.buf.length;
    while (cap < need) {
      cap *= 2;
    }
    const next = new Uint8Array(cap);
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }

  u8(n: number): void {
    this.reserve(1);
    this.buf[this.len++] = n;
  }

  u16be(n: number): void {
    this.reserve(2);
    new DataView(this.buf.buffer).setUint16(this.len, n, false);
    this.len += 2;
  }

  u16le(n: number): void {
    this.reserve(2);
    new DataView(this.buf.buffer).setUint16(this.len, n, true);
    this.len += 2;
  }

  u32be(n: number): void {
    this.reserve(4);
    new DataView(this.buf.buffer).setUint32(this.len, n, false);
    this.len += 4;
  }

  u32le(n: number): void {
    this.reserve(4);
    new DataView(this.buf.buffer).setUint32(this.len, n, true);
    this.len += 4;
  }

  u64be(n: bigint): void {
    this.reserve(8);
    new DataView(this.buf.buffer).setBigUint64(this.len, n, false);
    this.len += 8;
  }

  u64le(n: bigint): void {
    this.reserve(8);
    new DataView(this.buf.buffer).setBigUint64(this.len, n, true);
    this.len += 8;
  }

  u128be(n: bigint): void {
    this.u64be(n >> 64n);
    this.u64be(n & U64_MAX);
  }

  bytes(b: Uint8Array): void {
    this.reserve(b.length);
    this.buf.set(b, this.len);
    this.len += b.length;
  }

  /** A copy of what was written so far. */
  finish(): Uint8Array {
    return this.buf.slice(0, this.len);
  }
}

/** Bounds-checked input cursor. `take` returns `null` instead of reading past the end. */
export class Reader {
  private pos = 0;
  private readonly view: DataView;

  constructor(private readonly buf: Uint8Array) {
    this.view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  }

  get remaining(): number {
    return this.buf.length - this.pos;
  }

  /** The next `n` bytes as a copy, or `null` if fewer remain. */
  take(n: number): Uint8Array | null {
    if (n > this.remaining) {
      return null;
    }
    const out = this.buf.slice(this.pos, this.pos + n);
    this.pos += n;
    return out;
  }

  u8(): number | null {
    if (this.remaining < 1) {
      return null;
    }
    return this.buf[this.pos++] as number;
  }

  u16be(): number | null {
    if (this.remaining < 2) {
      return null;
    }
    const v = this.view.getUint16(this.pos, false);
    this.pos += 2;
    return v;
  }

  u16le(): number | null {
    if (this.remaining < 2) {
      return null;
    }
    const v = this.view.getUint16(this.pos, true);
    this.pos += 2;
    return v;
  }

  u32be(): number | null {
    if (this.remaining < 4) {
      return null;
    }
    const v = this.view.getUint32(this.pos, false);
    this.pos += 4;
    return v;
  }

  u32le(): number | null {
    if (this.remaining < 4) {
      return null;
    }
    const v = this.view.getUint32(this.pos, true);
    this.pos += 4;
    return v;
  }

  u64be(): bigint | null {
    if (this.remaining < 8) {
      return null;
    }
    const v = this.view.getBigUint64(this.pos, false);
    this.pos += 8;
    return v;
  }

  u64le(): bigint | null {
    if (this.remaining < 8) {
      return null;
    }
    const v = this.view.getBigUint64(this.pos, true);
    this.pos += 8;
    return v;
  }

  u128be(): bigint | null {
    const hi = this.u64be();
    const lo = this.u64be();
    if (hi === null || lo === null) {
      return null;
    }
    return (hi << 64n) | lo;
  }
}

#!/usr/bin/env python3
"""Seeds for crdt_text_decode, crdt_tree_decode and conflict_decode, encoded with Python struct.

They come from the byte layouts documented in crdt_text.rs, crdt_tree.rs and workflow.rs, never from
the Rust encoders. Run from any directory; --check verifies the tracked bytes without writing them.
"""
import argparse
from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parent


def hlc(wall, counter=0, node=1):
    return struct.pack('<QII', wall, counter, node)


def u16(n):
    return struct.pack('<H', n)


def u32(n):
    return struct.pack('<I', n)


def u64(n):
    return struct.pack('<Q', n)


def text_insert(wall, text, after=None, kind=1, version=1):
    out = bytes([version, kind]) + hlc(wall)
    if after is None:
        out += b'\x00'
    else:
        out += b'\x01' + hlc(after[0]) + u32(after[1])
    return out + u16(len(text)) + text


def text_delete(wall, target_wall, off):
    return b'\x01\x02' + hlc(wall) + hlc(target_wall) + u32(off)


def tree_move(wall, node, parent, pos, version=1):
    return bytes([version]) + hlc(wall) + u64(node) + u64(parent) + u64(pos)


def name(s):
    return u16(len(s)) + s


def proposal(wall, request, frm, to, actor):
    return hlc(wall) + u64(request) + name(frm) + name(to) + u64(actor)


def conflict(proposals, options, resolution=b'\x00', ordinal=1, version=1):
    out = bytes([version]) + (7).to_bytes(16, 'big') + u32(ordinal) + name(b'todo')
    out += u16(len(proposals)) + b''.join(proposals)
    out += u16(len(options)) + b''.join(options)
    return out + resolution


REVERT = b'\x00'


def choose(s):
    return b'\x01' + name(s)


def seeds():
    out = {}
    # Text operations: insert at the head, after an element, multi-byte text, delete.
    out['crdt_text_decode/insert-head'] = text_insert(2, b'hi')
    out['crdt_text_decode/insert-after'] = text_insert(3, b'x', after=(2, 1))
    out['crdt_text_decode/insert-multibyte'] = text_insert(3, 'café'.encode(), after=(2, 0))
    out['crdt_text_decode/delete'] = text_delete(5, 2, 1)
    out['crdt_text_decode/bad-version'] = text_insert(2, b'hi', version=2)
    out['crdt_text_decode/bad-kind'] = text_insert(2, b'hi', kind=3)
    out['crdt_text_decode/bad-flag'] = text_insert(2, b'hi')[:18] + b'\x02' + u16(2) + b'hi'
    out['crdt_text_decode/empty-text'] = text_insert(2, b'')
    out['crdt_text_decode/invalid-utf8'] = text_insert(2, b'\xff\xfe')
    out['crdt_text_decode/trailing-byte'] = text_insert(2, b'hi') + b'\x00'
    out['crdt_text_decode/truncated'] = text_insert(2, b'hi')[:-1]
    out['crdt_text_decode/non-causal-anchor'] = text_insert(2, b'x', after=(2, 0))
    out['crdt_text_decode/offset-past-chunk'] = text_insert(3, b'x', after=(2, 256))
    out['crdt_text_decode/zero-id'] = text_insert(2, b'hi')[:2] + b'\x00' * 16 + text_insert(2, b'hi')[18:]
    # Tree moves: a move, a delete (parent = trash), a move under the root, and refusals.
    out['crdt_tree_decode/move'] = tree_move(4, 10, 0, 3)
    out['crdt_tree_decode/delete-to-trash'] = tree_move(5, 10, 1, 0)
    out['crdt_tree_decode/extreme-position'] = tree_move(6, 2 ** 64 - 1, 10, 2 ** 64 - 1)
    out['crdt_tree_decode/bad-version'] = tree_move(4, 10, 0, 3, version=2)
    out['crdt_tree_decode/reserved-root'] = tree_move(4, 0, 10, 3)
    out['crdt_tree_decode/reserved-trash'] = tree_move(4, 1, 10, 3)
    out['crdt_tree_decode/short'] = tree_move(4, 10, 0, 3)[:-1]
    out['crdt_tree_decode/long'] = tree_move(4, 10, 0, 3) + b'\x00'
    # Conflicts. Proposals sort by (hlc, request id, from, to, actor); options are Revert then
    # one Choose per distinct target, sorted by name.
    p_done = proposal(10, 1, b'todo', b'done', 100)
    p_blocked = proposal(11, 2, b'todo', b'blocked', 200)
    options = [REVERT, choose(b'blocked'), choose(b'done')]
    out['conflict_decode/open'] = conflict([p_done, p_blocked], options)
    out['conflict_decode/resolved-choice'] = conflict(
        [p_done, p_blocked], options, b'\x01' + choose(b'done') + u64(9))
    out['conflict_decode/resolved-revert'] = conflict(
        [p_done, p_blocked], options, b'\x01' + REVERT + u64(9))
    out['conflict_decode/same-target'] = conflict(
        [p_done, proposal(11, 2, b'todo', b'done', 200)], [REVERT, choose(b'done')])
    out['conflict_decode/bad-version'] = conflict([p_done, p_blocked], options, version=2)
    out['conflict_decode/zero-ordinal'] = conflict([p_done, p_blocked], options, ordinal=0)
    out['conflict_decode/one-proposal'] = conflict([p_done], [REVERT, choose(b'done')])
    out['conflict_decode/unsorted-proposals'] = conflict([p_blocked, p_done], options)
    out['conflict_decode/options-mismatch'] = conflict([p_done, p_blocked], [REVERT, choose(b'done')])
    out['conflict_decode/resolution-not-an-option'] = conflict(
        [p_done, p_blocked], options, b'\x01' + choose(b'review') + u64(9))
    out['conflict_decode/trailing-byte'] = conflict([p_done, p_blocked], options) + b'\x00'
    out['conflict_decode/truncated'] = conflict([p_done, p_blocked], options)[:-3]
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    expected = seeds()
    for rel, value in expected.items():
        path = ROOT / rel
        if args.check:
            if not path.is_file() or path.read_bytes() != value:
                raise SystemExit(f'seed differs: {rel}')
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(value)
    print(f'{len(expected)} independent crdt and workflow seeds ' + ('verified' if args.check else 'written'))


if __name__ == '__main__':
    main()

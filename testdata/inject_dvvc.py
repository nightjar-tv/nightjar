#!/usr/bin/env python3
"""Insert a dvvC box into the first hvc1 sample entry after hvcC."""
import struct
import sys
from pathlib import Path

CONTAINERS = frozenset(
    {b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"hvc1", b"hev1"}
)


def u32be(b: bytes) -> int:
    return struct.unpack(">I", b)[0]


def p32be(n: int) -> bytes:
    return struct.pack(">I", n)


def box_size(data: bytes, pos: int) -> int:
    sz = u32be(data[pos : pos + 4])
    if sz == 1:
        return struct.unpack(">Q", data[pos + 8 : pos + 16])[0]
    if sz == 0:
        return len(data) - pos
    return sz


def child_start(pos: int, typ: bytes) -> int:
    if typ == b"stsd":
        return pos + 16
    if typ in (b"hvc1", b"hev1"):
        return pos + 8 + 78
    return pos + 8


def find_hvcc_insert(data: bytes):
    pos = 0
    end = len(data)
    while pos + 8 <= end:
        typ = bytes(data[pos + 4 : pos + 8])
        sz = box_size(data, pos)
        if typ == b"moov":
            return _walk(data, child_start(pos, typ), pos + sz, [pos])
        pos += sz
    return None


def _walk(data: bytes, start: int, end: int, ancestors: list):
    pos = start
    while pos + 8 <= end:
        typ = bytes(data[pos + 4 : pos + 8])
        sz = box_size(data, pos)
        box_end = pos + sz
        if typ == b"hvcC":
            return ancestors, box_end
        path = ancestors + [pos]
        if typ in CONTAINERS:
            found = _walk(data, child_start(pos, typ), box_end, path)
            if found:
                return found
        pos = box_end
    return None


def patch(path: Path, dvvc: bytes) -> None:
    data = bytearray(path.read_bytes())
    if data.find(b"dvvC") >= 0:
        print(f"{path}: dvvC already present")
        return
    located = find_hvcc_insert(data)
    if located is None:
        raise SystemExit("hvcC not found")
    ancestors, insert_at = located
    data[insert_at:insert_at] = dvvc
    delta = len(dvvc)
    for box_start in ancestors:
        old = box_size(data, box_start)
        data[box_start : box_start + 4] = p32be(old + delta)
    path.write_bytes(data)
    print(
        f"{path}: inserted dvvC ({len(dvvc)} bytes), updated {len(ancestors)} ancestor boxes"
    )


if __name__ == "__main__":
    patch(Path(sys.argv[1]), bytes.fromhex(sys.argv[2]))

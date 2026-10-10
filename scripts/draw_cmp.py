#!/usr/bin/env python3
"""绘制对拍：逐字节比较 ref/rust 的 u8 dump（[n][data]）。"""
import struct
import sys


def load(path):
    with open(path, "rb") as f:
        data = f.read()
    (n,) = struct.unpack_from("<i", data, 0)
    return n, data[4:4 + n]


def main():
    ref_n, ref = load(sys.argv[1])
    rust_n, rust = load(sys.argv[2])
    if ref_n != rust_n:
        print(f"MISMATCH: ref n={ref_n} rust n={rust_n}")
        sys.exit(1)
    if ref == rust:
        print(f"drawing EXACT MATCH ({ref_n} bytes)")
        return
    diff = sum(1 for a, b in zip(ref, rust) if a != b)
    print(f"MISMATCH: {diff}/{ref_n} bytes differ")
    # 首个差异位置
    for i, (a, b) in enumerate(zip(ref, rust)):
        if a != b:
            print(f"  first diff at byte {i} (pixel {i}, row {i // max(1, (ref_n and int(ref_n ** 0.5)) or 1)}): ref={a} rust={b}")
            break
    sys.exit(1)


if __name__ == "__main__":
    main()

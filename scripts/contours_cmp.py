#!/usr/bin/env python3
"""find_contours 对拍比较：逐位对比两个轮廓 dump（i32 LE 流）。

格式：[n][len_0][x y ...][len_1]...[hier_n][next prev fc parent]*n
（hier 部分可选，两文件须一致存在或一致不存在）

用法: contours_cmp.py <ref.bin> <rust.bin>
"""

import struct
import sys


def read_i32s(path):
    with open(path, "rb") as f:
        data = f.read()
    if len(data) % 4 != 0:
        raise ValueError(f"{path}: length {len(data)} not divisible by 4")
    return struct.unpack(f"<{len(data)//4}i", data), len(data)


def parse(vals):
    it = iter(vals)
    n = next(it)
    contours = []
    for _ in range(n):
        ln = next(it)
        pts = [(next(it), next(it)) for _ in range(ln)]
        contours.append(pts)
    hier = None
    rest = list(it)
    if rest:
        hn = rest[0]
        hier = [tuple(rest[1 + 4 * i:5 + 4 * i]) for i in range(hn)]
    return contours, hier


def main():
    ref, ref_len = read_i32s(sys.argv[1])
    rust, rust_len = read_i32s(sys.argv[2])

    if ref_len == rust_len and ref == rust:
        print(f"contours EXACT MATCH ({ref_len} bytes)")
        return 0

    rc, rh = parse(ref)
    tc, th = parse(rust)
    print(f"MISMATCH: ref {len(rc)} contours, rust {len(tc)} contours")
    for i, (a, b) in enumerate(zip(rc, tc)):
        if a != b:
            print(f"  contour[{i}]: ref len={len(a)} rust len={len(b)}")
            for j, (pa, pb) in enumerate(zip(a, b)):
                if pa != pb:
                    print(f"    first diff at pt[{j}]: ref={pa} rust={pb}")
                    break
            else:
                if len(a) != len(b):
                    print(f"    prefix matches, length differs")
            if i > 5:
                print("  ...")
                break
    if rh is not None or th is not None:
        if rh != th:
            print(f"  hierarchy mismatch: ref={rh is not None} rust={th is not None}")
            if rh and th:
                for i, (a, b) in enumerate(zip(rh, th)):
                    if a != b:
                        print(f"    hier[{i}]: ref={a} rust={b}")
                        break
    return 1


if __name__ == "__main__":
    sys.exit(main())

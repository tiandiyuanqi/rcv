#!/usr/bin/env python3
"""逐像素 PSNR 计算。输入为两个原始像素 dump（无头），要求长度一致、类型一致。

纯 Python 实现（无 numpy 依赖）。用法：
    psnr.py <ref.bin> <rust.bin> [--dtype u8|u16|i16|f32]
"""

import math
import struct
import sys

PASS_THRESHOLD = 60.0

DTYPES = {
    "u8": (1, "B"),
    "u16": (2, "H"),
    "i16": (2, "H"),  # 按位模式读（差值与符号无关）
    "f32": (4, "f"),
}

MAX_VAL = {"u8": 255.0, "u16": 65535.0, "i16": 65535.0, "f32": 1.0}


def read_values(path: str, dtype: str):
    size, fmt = DTYPES[dtype]
    with open(path, "rb") as f:
        data = f.read()
    n = len(data) // size
    if len(data) % size != 0:
        raise ValueError(f"{path}: length {len(data)} not divisible by {size}")
    return struct.unpack(f"<{n}{fmt}", data)


def psnr(a, b, max_val: float) -> float:
    if len(a) != len(b):
        raise ValueError(f"length mismatch: {len(a)} vs {len(b)}")
    if len(a) == 0:
        return float("inf")
    mse = sum((x - y) ** 2 for x, y in zip(a, b)) / len(a)
    if mse == 0.0:
        return float("inf")
    return 10.0 * math.log10((max_val * max_val) / mse)


def main() -> int:
    args = sys.argv[1:]
    dtype = "u8"
    if "--dtype" in args:
        i = args.index("--dtype")
        dtype = args[i + 1]
        del args[i : i + 2]
    if len(args) != 2:
        print(f"usage: {sys.argv[0]} <ref.bin> <rust.bin> [--dtype u8|u16|f32]", file=sys.stderr)
        return 2

    ref_path, rust_path = args
    a = read_values(ref_path, dtype)
    b = read_values(rust_path, dtype)

    value = psnr(a, b, MAX_VAL[dtype])

    status = "PASS" if value >= PASS_THRESHOLD else "FAIL"
    print(f"PSNR = {value:.3f} dB  (threshold {PASS_THRESHOLD} dB)  [{status}]")
    return 0 if status == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
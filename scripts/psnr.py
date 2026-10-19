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


def psnr_rel(a, b) -> float:
    """相对误差 PSNR：以 ref 的绝对值为基准，适合大数值输出（如 matchTemplate 非归一化方法）。

    为避免近零值（如 SqDiff 的完美匹配点）导致相对误差发散，分母下限取
    max(|a|, 1e-3 * max|a|)，即相对 ref 动态范围的 0.1%。
    """
    if len(a) != len(b):
        raise ValueError(f"length mismatch: {len(a)} vs {len(b)}")
    if len(a) == 0:
        return float("inf")
    floor = 1e-3 * max(abs(x) for x in a)
    # 相对误差：|a-b| / max(|a|, floor)
    mse = sum(((x - y) / max(abs(x), floor)) ** 2 for x, y in zip(a, b)) / len(a)
    if mse == 0.0:
        return float("inf")
    return 10.0 * math.log10(1.0 / mse)


def main() -> int:
    args = sys.argv[1:]
    dtype = "u8"
    rel = False
    if "--dtype" in args:
        i = args.index("--dtype")
        dtype = args[i + 1]
        del args[i : i + 2]
    if "--rel" in args:
        rel = True
        args.remove("--rel")
    if len(args) != 2:
        print(f"usage: {sys.argv[0]} <ref.bin> <rust.bin> [--dtype u8|u16|f32] [--rel]", file=sys.stderr)
        return 2

    ref_path, rust_path = args
    a = read_values(ref_path, dtype)
    b = read_values(rust_path, dtype)

    value = psnr_rel(a, b) if rel else psnr(a, b, MAX_VAL[dtype])

    status = "PASS" if value >= PASS_THRESHOLD else "FAIL"
    print(f"PSNR = {value:.3f} dB  (threshold {PASS_THRESHOLD} dB)  [{status}]")
    return 0 if status == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
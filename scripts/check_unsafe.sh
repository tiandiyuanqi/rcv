#!/usr/bin/env bash
# 检查 simd_kernels/ 之外无 unsafe 块。发现即失败。
# 允许在 simd_kernels/ 内使用 unsafe，但每处必须附 // SAFETY: 注释。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="${ROOT}/src"

if [[ ! -d "${SRC}" ]]; then
    echo "[check_unsafe] src/ not found, skip."
    exit 0
fi

FAIL=0

# 1. simd_kernels/ 之外出现 unsafe 关键字（含 unsafe fn / unsafe impl / unsafe { }）
while IFS= read -r -d '' f; do
    # 跳过 simd_kernels 目录
    case "${f}" in
        *"/simd_kernels/"*) continue ;;
    esac
    if grep -nE '\bunsafe\b' "${f}" >/dev/null; then
        echo "[check_unsafe] VIOLATION: unsafe outside simd_kernels/: ${f}"
        grep -nE '\bunsafe\b' "${f}" || true
        FAIL=1
    fi
done < <(find "${SRC}" -type f -name '*.rs' -print0)

# 2. simd_kernels/ 内的 unsafe 必须紧邻 // SAFETY: 注释
if [[ -d "${SRC}/simd_kernels" ]]; then
    while IFS= read -r -d '' f; do
        # 逐行检查：出现 unsafe 的行，向上找 3 行内是否有 // SAFETY:
        awk '
            { lines[NR]=$0 }
            END {
                for (i=1; i<=NR; i++) {
                    if (lines[i] ~ /\bunsafe\b/) {
                        found = 0
                        for (j=i-1; j>=i-3 && j>=1; j--) {
                            if (lines[j] ~ /\/\/ SAFETY:/) { found = 1; break }
                        }
                        if (!found) {
                            printf "[check_unsafe] MISSING SAFETY: %s:%d: %s\n",
                                   FILENAME, i, lines[i]
                            exit 1
                        }
                    }
                }
            }
        ' "${f}" || FAIL=1
    done < <(find "${SRC}/simd_kernels" -type f -name '*.rs' -print0)
fi

if [[ "${FAIL}" -ne 0 ]]; then
    echo "[check_unsafe] FAILED"
    exit 1
fi

echo "[check_unsafe] OK"
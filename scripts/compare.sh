#!/usr/bin/env bash
# 唯一对拍入口：编译 ref -> 跑 ref -> 跑 rust -> 算 PSNR -> 输出报告。
#
# 用法:
#   scripts/compare.sh <func> <input.png> [args...]
#
# 例:
#   scripts/compare.sh to_grayscale compare/inputs/lena.png
#
# 依赖:
#   - OpenCV5 安装于 /opt/opencv5（world 单库，链接 -lopencv_world）
#   - g++ 支持 C++17
#   - python3（用于 psnr.py）

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OPENCV_DIR="${OPENCV_DIR:-/opt/opencv5}"
CXX="${CXX:-g++}"
REF_BIN="${ROOT}/compare/opencv5_ref"
REF_SRC="${ROOT}/compare/compare_opencv5.cpp"
REF_OUT_DIR="${ROOT}/compare/ref"
RUST_OUT_DIR="${ROOT}/compare/rust"

mkdir -p "${REF_OUT_DIR}" "${RUST_OUT_DIR}"

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <func> <input.png> [args...]" >&2
    exit 2
fi

FUNC="$1"; shift
INPUT="$1"; shift

# 参数哈希，用于区分不同参数的 ref 输出
PARAM_HASH="$(printf '%s' "$*" | sha1sum | cut -c1-12)"
REF_OUT="${REF_OUT_DIR}/${FUNC}_${PARAM_HASH}.bin"
RUST_OUT="${RUST_OUT_DIR}/${FUNC}_${PARAM_HASH}.bin"

# 1. 编译 ref（源文件变更时才重编）
if [[ ! -x "${REF_BIN}" || "${REF_SRC}" -nt "${REF_BIN}" ]]; then
    echo "[compare] building opencv5_ref ..."
    "${CXX}" -std=c++17 -O3 "${REF_SRC}" -o "${REF_BIN}" \
        -I"${OPENCV_DIR}/include/opencv5" \
        -L"${OPENCV_DIR}/lib" \
        -lopencv_world
fi

# 2. 跑 ref
echo "[compare] running ref: ${FUNC} ${INPUT} $*"
LD_LIBRARY_PATH="${OPENCV_DIR}/lib:${LD_LIBRARY_PATH:-}" \
    "${REF_BIN}" "${FUNC}" "${INPUT}" "${REF_OUT}" "$@"

# 3. 跑 rust
echo "[compare] running rust: ${FUNC} ${INPUT} $*"
cargo run --release --example compare -- \
    "${FUNC}" "${INPUT}" "${RUST_OUT}" "$@"

# 4. 算 PSNR（DTYPE 环境变量指定像素类型，默认 u8）
echo "[compare] computing PSNR ..."
DTYPE="${DTYPE:-u8}"
REL_FLAG=""
# matchTemplate 非归一化方法（0/2/4）输出大数值，用相对误差评估
if [[ "${FUNC}" == "match_template" && "${DTYPE}" == "f32" ]]; then
    case "$1" in
        0|2|4) REL_FLAG="--rel" ;;
    esac
fi
python3 "${ROOT}/scripts/psnr.py" "${REF_OUT}" "${RUST_OUT}" --dtype "${DTYPE}" ${REL_FLAG}

echo "[compare] done. ref=${REF_OUT} rust=${RUST_OUT}"
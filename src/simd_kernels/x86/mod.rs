//! x86 SIMD 内核（AVX2 + AVX512）。
//!
//! 运行时派发：`AVX512 → AVX2 → 标量`，用 `is_x86_feature_detected!` 探测并缓存（`OnceLock`）。

pub mod avx2;
pub mod avx512;

use std::sync::OnceLock;

/// 探测到的最高可用指令集（缓存一次）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isa {
    Avx512,
    Avx2,
    Scalar,
}

static ISA: OnceLock<Isa> = OnceLock::new();

/// 返回当前 CPU 可用的最高指令集。
pub fn isa() -> Isa {
    *ISA.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx512f")
                && std::arch::is_x86_feature_detected!("avx512bw")
                && std::arch::is_x86_feature_detected!("avx512vl")
            {
                return Isa::Avx512;
            }
            if std::arch::is_x86_feature_detected!("avx2") {
                return Isa::Avx2;
            }
        }
        Isa::Scalar
    })
}

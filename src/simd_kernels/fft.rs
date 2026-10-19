//! FFT 手写 SIMD 内核（AVX2，运行时派发）。
//!
//! radix-2 迭代 FFT，一次处理 4 个蝶形（8 个 f32 = 1 个 AVX2 寄存器）。
//! 数据为 AoS 布局（`Complex` = re, im 交错），蝶形内用 shuffle 提取 re/im。
//!
//! 参考 OpenCV5 `dxt.cpp` 的 radix-2 蝶形，但用 AVX2 向量化。

use crate::simd_kernels::x86;

/// 一维 FFT（radix-2 迭代，Cooley-Tukey），AVX2 加速。
///
/// `data` 为 `n` 个复数（AoS：re, im 交错），`n` 必须是 2 的幂。原地变换。
/// 正变换（无缩放）。
pub fn fft1d(data: &mut [f32]) {
    let n = data.len() / 2;
    debug_assert!(n.is_power_of_two(), "FFT size must be a power of two, got {n}");
    if n <= 1 {
        return;
    }
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::fft1d_avx2(data, n)
        },
        x86::Isa::Scalar => scalar_fft1d(data, n),
    }
}

/// 一维逆 FFT（radix-2 迭代），带 1/n 缩放。
pub fn ifft1d(data: &mut [f32]) {
    let n = data.len() / 2;
    debug_assert!(n.is_power_of_two(), "IFFT size must be a power of two, got {n}");
    if n <= 1 {
        return;
    }
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::ifft1d_avx2(data, n)
        },
        x86::Isa::Scalar => scalar_ifft1d(data, n),
    }
}

/// SoA 一维 FFT（re/im 分离，AVX2 加速）。`re`/`im` 各 `n` 个 f32，`n` 为 2 的幂。
pub fn fft1d_soa(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two(), "FFT size must be a power of two, got {n}");
    debug_assert_eq!(im.len(), n);
    if n <= 1 {
        return;
    }
    let plan = FftPlan::new(n);
    fft1d_soa_plan(re, im, &plan);
}

/// SoA 一维逆 FFT（re/im 分离，AVX2 加速），带 1/n 缩放。
pub fn ifft1d_soa(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two(), "IFFT size must be a power of two, got {n}");
    debug_assert_eq!(im.len(), n);
    if n <= 1 {
        return;
    }
    let plan = FftPlan::new(n);
    ifft1d_soa_plan(re, im, &plan);
}

/// SoA 标量一维 FFT（正确性基准）。
pub fn scalar_fft1d_soa(re: &mut [f32], im: &mut [f32], n: usize) {
    bit_reverse_soa(re, im, n);
    let mut len = 2usize;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let wlen_re = ang.cos();
        let wlen_im = ang.sin();
        let half = len >> 1;
        for i in (0..n).step_by(len) {
            let mut w_re = 1.0f32;
            let mut w_im = 0.0f32;
            for k in 0..half {
                let u_re = re[i + k];
                let u_im = im[i + k];
                let v_re = re[i + k + half];
                let v_im = im[i + k + half];
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                re[i + k] = u_re + vw_re;
                im[i + k] = u_im + vw_im;
                re[i + k + half] = u_re - vw_re;
                im[i + k + half] = u_im - vw_im;
                let nw_re = w_re * wlen_re - w_im * wlen_im;
                let nw_im = w_re * wlen_im + w_im * wlen_re;
                w_re = nw_re;
                w_im = nw_im;
            }
        }
        len <<= 1;
    }
}

/// SoA 标量一维逆 FFT（正确性基准），带 1/n 缩放。
pub fn scalar_ifft1d_soa(re: &mut [f32], im: &mut [f32], n: usize) {
    bit_reverse_soa(re, im, n);
    let mut len = 2usize;
    while len <= n {
        let ang = 2.0 * std::f32::consts::PI / len as f32;
        let wlen_re = ang.cos();
        let wlen_im = ang.sin();
        let half = len >> 1;
        for i in (0..n).step_by(len) {
            let mut w_re = 1.0f32;
            let mut w_im = 0.0f32;
            for k in 0..half {
                let u_re = re[i + k];
                let u_im = im[i + k];
                let v_re = re[i + k + half];
                let v_im = im[i + k + half];
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                re[i + k] = u_re + vw_re;
                im[i + k] = u_im + vw_im;
                re[i + k + half] = u_re - vw_re;
                im[i + k + half] = u_im - vw_im;
                let nw_re = w_re * wlen_re - w_im * wlen_im;
                let nw_im = w_re * wlen_im + w_im * wlen_re;
                w_re = nw_re;
                w_im = nw_im;
            }
        }
        len <<= 1;
    }
    let inv = 1.0 / n as f32;
    for v in re.iter_mut() {
        *v *= inv;
    }
    for v in im.iter_mut() {
        *v *= inv;
    }
}

/// SoA 位反转置换（`re`/`im` 各 `n` 个 f32）。
fn bit_reverse_soa(re: &mut [f32], im: &mut [f32], n: usize) {
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
}

/// 标量一维 FFT（正确性基准）。
pub fn scalar_fft1d(data: &mut [f32], n: usize) {
    bit_reverse(data, n);
    let mut len = 2usize;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let wlen_re = ang.cos();
        let wlen_im = ang.sin();
        let half = len >> 1;
        for i in (0..n).step_by(len) {
            let mut w_re = 1.0f32;
            let mut w_im = 0.0f32;
            for k in 0..half {
                let u_re = data[2 * (i + k)];
                let u_im = data[2 * (i + k) + 1];
                let v_re = data[2 * (i + k + half)];
                let v_im = data[2 * (i + k + half) + 1];
                // v * w
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                data[2 * (i + k)] = u_re + vw_re;
                data[2 * (i + k) + 1] = u_im + vw_im;
                data[2 * (i + k + half)] = u_re - vw_re;
                data[2 * (i + k + half) + 1] = u_im - vw_im;
                // w *= wlen
                let nw_re = w_re * wlen_re - w_im * wlen_im;
                let nw_im = w_re * wlen_im + w_im * wlen_re;
                w_re = nw_re;
                w_im = nw_im;
            }
        }
        len <<= 1;
    }
}

/// 标量一维逆 FFT（正确性基准）。
pub fn scalar_ifft1d(data: &mut [f32], n: usize) {
    bit_reverse(data, n);
    let mut len = 2usize;
    while len <= n {
        let ang = 2.0 * std::f32::consts::PI / len as f32;
        let wlen_re = ang.cos();
        let wlen_im = ang.sin();
        let half = len >> 1;
        for i in (0..n).step_by(len) {
            let mut w_re = 1.0f32;
            let mut w_im = 0.0f32;
            for k in 0..half {
                let u_re = data[2 * (i + k)];
                let u_im = data[2 * (i + k) + 1];
                let v_re = data[2 * (i + k + half)];
                let v_im = data[2 * (i + k + half) + 1];
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                data[2 * (i + k)] = u_re + vw_re;
                data[2 * (i + k) + 1] = u_im + vw_im;
                data[2 * (i + k + half)] = u_re - vw_re;
                data[2 * (i + k + half) + 1] = u_im - vw_im;
                let nw_re = w_re * wlen_re - w_im * wlen_im;
                let nw_im = w_re * wlen_im + w_im * wlen_re;
                w_re = nw_re;
                w_im = nw_im;
            }
        }
        len <<= 1;
    }
    let inv = 1.0 / n as f32;
    for v in data.iter_mut() {
        *v *= inv;
    }
}

/// 位反转置换（AoS 布局，`data` 为 `2n` 个 f32）。
fn bit_reverse(data: &mut [f32], n: usize) {
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            data.swap(2 * i, 2 * j);
            data.swap(2 * i + 1, 2 * j + 1);
        }
    }
}

/// 预计算的 FFT 旋转因子表（SoA + AoS 双布局）。
///
/// 级 `len`（2, 4, ..., n）的因子存储在拼接布局中：
/// SoA 偏移 = `len/2 - 1`，AoS 偏移 = `2*(len/2 - 1)`。
/// 预计算一次后，任意多行 FFT 共享，避免每行每级重复计算 cos/sin。
pub struct FftPlan {
    /// 正变换因子（-2πi/len 方向，SoA：re/im 分离）。
    pub wre: Vec<f32>,
    pub wim: Vec<f32>,
    /// 逆变换因子（+2πi/len 方向，SoA）。
    pub inv_wre: Vec<f32>,
    pub inv_wim: Vec<f32>,
    /// 正变换因子（AoS：re, im 交错）。
    pub aos_w: Vec<f32>,
    /// 逆变换因子（AoS）。
    pub aos_inv_w: Vec<f32>,
    /// 位反转置换表：`br[i]` 为 i 的位反转（n 位）。
    /// 仅列出 `i < br[i]` 的交换对（i, br[i]），避免重复交换。
    pub br_pairs: Vec<(u32, u32)>,
}

impl FftPlan {
    /// 为 `n` 点 FFT 预计算旋转因子。`n` 必须是 2 的幂。
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 2, "FFT size must be a power of two, got {n}");
        // 拼接布局总大小：sum(len/2 for len in 2,4..n) = n - 1，取 n 对齐。
        let mut wre = vec![0.0f32; n];
        let mut wim = vec![0.0f32; n];
        let mut inv_wre = vec![0.0f32; n];
        let mut inv_wim = vec![0.0f32; n];
        let mut aos_w = vec![0.0f32; 2 * n];
        let mut aos_inv_w = vec![0.0f32; 2 * n];
        let mut off = 0usize;
        let mut len = 2usize;
        while len <= n {
            let h = len >> 1;
            let ang = -2.0 * std::f32::consts::PI / len as f32;
            for k in 0..h {
                let a = ang * k as f32;
                let (c, s) = (a.cos(), a.sin());
                wre[off + k] = c;
                wim[off + k] = s;
                inv_wre[off + k] = c;
                inv_wim[off + k] = -s;
                aos_w[2 * (off + k)] = c;
                aos_w[2 * (off + k) + 1] = s;
                aos_inv_w[2 * (off + k)] = c;
                aos_inv_w[2 * (off + k) + 1] = -s;
            }
            off += h;
            len <<= 1;
        }
        // 位反转交换对（i < br[i]）
        let bits = n.trailing_zeros();
        let mut br_pairs = Vec::new();
        for i in 0..n as u32 {
            let br = i.reverse_bits() >> (32 - bits);
            if i < br {
                br_pairs.push((i, br));
            }
        }
        FftPlan { wre, wim, inv_wre, inv_wim, aos_w, aos_inv_w, br_pairs }
    }
}

/// SoA 一维 FFT（用预计算 plan，AVX2 加速）。
///
/// `re`/`im` 各 `n` 个 f32，`n` 为 2 的幂且与 plan 尺寸一致。
pub fn fft1d_soa_plan(re: &mut [f32], im: &mut [f32], plan: &FftPlan) {
    let n = re.len();
    debug_assert!(n.is_power_of_two(), "FFT size must be a power of two, got {n}");
    debug_assert_eq!(im.len(), n);
    debug_assert!(n >= 2 && plan.wre.len() >= n, "plan size mismatch");
    if n <= 1 {
        return;
    }
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::fft1d_soa_plan_avx2(re, im, n, plan.wre.as_ptr(), plan.wim.as_ptr(), plan.br_pairs.as_slice())
        },
        x86::Isa::Scalar => scalar_fft1d_soa(re, im, n),
    }
}

/// SoA 一维逆 FFT（用预计算 plan，AVX2 加速）。
///
/// `scale` 为输出缩放（如 1/n）；传 1.0 跳过缩放（二维 IFFT 中由外层统一缩放）。
pub fn ifft1d_soa_plan_scaled(re: &mut [f32], im: &mut [f32], plan: &FftPlan, scale: f32) {
    let n = re.len();
    debug_assert!(n.is_power_of_two(), "IFFT size must be a power of two, got {n}");
    debug_assert_eq!(im.len(), n);
    debug_assert!(n >= 2 && plan.inv_wre.len() >= n, "plan size mismatch");
    if n <= 1 {
        return;
    }
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::ifft1d_soa_plan_avx2(re, im, n, plan.inv_wre.as_ptr(), plan.inv_wim.as_ptr(), plan.br_pairs.as_slice())
        },
        x86::Isa::Scalar => scalar_ifft1d_soa(re, im, n),
    }
    if scale != 1.0 {
        for v in re.iter_mut() { *v *= scale; }
        for v in im.iter_mut() { *v *= scale; }
    }
}

/// SoA 一维逆 FFT（用预计算 plan，AVX2 加速），带 1/n 缩放。
pub fn ifft1d_soa_plan(re: &mut [f32], im: &mut [f32], plan: &FftPlan) {
    let n = re.len();
    ifft1d_soa_plan_scaled(re, im, plan, 1.0 / n as f32);
}

/// AoS 一维 FFT（用预计算 plan，AVX2 加速）。`data` 为 `2n` 个 f32（re, im 交错）。
pub fn fft1d_aos_plan(data: &mut [f32], plan: &FftPlan) {
    let n = data.len() / 2;
    debug_assert!(n.is_power_of_two(), "FFT size must be a power of two, got {n}");
    debug_assert!(n >= 2 && plan.aos_w.len() >= 2 * n, "plan size mismatch");
    if n <= 1 {
        return;
    }
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::fft1d_aos_plan_avx2(data, n, plan.aos_w.as_ptr())
        },
        x86::Isa::Scalar => scalar_fft1d(data, n),
    }
}

/// AoS 一维逆 FFT（用预计算 plan，AVX2 加速），带 1/n 缩放。
pub fn ifft1d_aos_plan(data: &mut [f32], plan: &FftPlan) {
    let n = data.len() / 2;
    debug_assert!(n.is_power_of_two(), "IFFT size must be a power of two, got {n}");
    debug_assert!(n >= 2 && plan.aos_inv_w.len() >= 2 * n, "plan size mismatch");
    if n <= 1 {
        return;
    }
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::ifft1d_aos_plan_avx2(data, n, plan.aos_inv_w.as_ptr())
        },
        x86::Isa::Scalar => scalar_ifft1d(data, n),
    }
}
/// 方阵原地转置（SoA 布局，8x8 分块，AVX2 加速）。
///
/// `re`/`im` 为 `n*n` 个 f32。n 为 8 的倍数时走 AVX2 内核，
/// 否则回退标量分块转置。
pub fn transpose_square_soa(re: &mut [f32], im: &mut [f32], n: usize) {
    if n % 8 == 0 && n >= 16 {
        match x86::isa() {
            x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
                // 块行独立，多线程池时按块行并行（每块行含与下三角的交换，
                // 块对 (by,bx) 只在其 by 行处理，无竞争）。
                let nb = n / 8;
                let nthreads = rayon::current_num_threads();
                // 每块至少 8 个块行（64 行）才值得并行
                const MIN_BLOCK_ROWS: usize = 8;
                if nthreads > 1 && nb >= MIN_BLOCK_ROWS * 2 {
                    use rayon::prelude::*;
                    let max_blocks = (nb / MIN_BLOCK_ROWS).min(nthreads * 4);
                    let rows_per_block = nb.div_ceil(max_blocks);
                    let nblocks = nb.div_ceil(rows_per_block);
                    // 原地转置：块行跨全矩阵，无法用 par_chunks_mut，
                    // 用原始指针分片（块行范围互不重叠）
                    // SAFETY 包装：裸指针跨线程传递（块行范围互不重叠，见下）
                    #[derive(Clone, Copy)]
                    struct SendPtr(*mut f32);
                    unsafe impl Send for SendPtr {}
                    unsafe impl Sync for SendPtr {}
                    // 按值传整个 SendPtr，强制闭包捕获完整结构体（Send+Sync）
                    // SAFETY: 块行范围互不重叠（每块行只写自己的行和对应
                    // 列块，块对唯一归属），n*n 范围内
                    fn run_rows(
                        re_p: SendPtr,
                        im_p: SendPtr,
                        n: usize,
                        b0: usize,
                        b1: usize,
                    ) {
                        // SAFETY: 块行范围互不重叠（见上），指针来自调用方切片
                        unsafe {
                            let re = std::slice::from_raw_parts_mut(re_p.0, n * n);
                            let im = std::slice::from_raw_parts_mut(im_p.0, n * n);
                            x86::avx2::transpose_block_rows_avx2(re, im, n, b0, b1);
                        }
                    }
                    let re_ptr = SendPtr(re.as_mut_ptr());
                    let im_ptr = SendPtr(im.as_mut_ptr());
                    (0..nblocks).into_par_iter().for_each(move |bi| {
                        let b0 = bi * rows_per_block;
                        let b1 = (b0 + rows_per_block).min(nb);
                        run_rows(re_ptr, im_ptr, n, b0, b1);
                    });
                } else {
                    x86::avx2::transpose_square_soa_avx2(re, im, n)
                }
            },
            x86::Isa::Scalar => transpose_square_soa_scalar(re, im, n),
        }
    } else {
        transpose_square_soa_scalar(re, im, n);
    }
}

/// 标量 8x8 分块原地转置（SoA 布局）。
fn transpose_square_soa_scalar(re: &mut [f32], im: &mut [f32], n: usize) {
    const BLOCK: usize = 8;
    let nb = n.div_ceil(BLOCK);
    for by in 0..nb {
        for bx in 0..nb {
            let y0 = by * BLOCK;
            let x0 = bx * BLOCK;
            let y1 = (y0 + BLOCK).min(n);
            let x1 = (x0 + BLOCK).min(n);
            if by == bx {
                for i in y0..y1 {
                    for j in x0..x1 {
                        if i < j {
                            re.swap(i * n + j, j * n + i);
                            im.swap(i * n + j, j * n + i);
                        }
                    }
                }
            } else if by < bx {
                for i in y0..y1 {
                    for j in x0..x1 {
                        re.swap(i * n + j, j * n + i);
                        im.swap(i * n + j, j * n + i);
                    }
                }
            }
        }
    }
}

/// 频域互相关解包（AVX2 派发）：由 RFFT 打包频谱直接算 corr_spec。
pub fn ccorr_unpack(z_re: &mut [f32], z_im: &mut [f32], dw: usize, dh: usize) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 if dw >= 16 && dh >= 16 => unsafe {
            x86::avx2::ccorr_unpack_avx2(z_re, z_im, dw, dh)
        },
        _ => {
            // 标量回退
            for y in 0..dh {
                for x in 0..dw {
                    let idx = y * dw + x;
                    let my = (dh - y) % dh;
                    let mx = (dw - x) % dw;
                    let midx = my * dw + mx;
                    if midx < idx { continue; }
                    let (zk_re, zk_im) = (z_re[idx], z_im[idx]);
                    let (zm_re, zm_im) = (z_re[midx], z_im[midx]);
                    let p_im = zk_re * zm_im + zk_im * zm_re;
                    let c_re = 0.5 * p_im;
                    let c_im = 0.25 * (zk_re * zk_re + zk_im * zk_im - zm_re * zm_re - zm_im * zm_im);
                    if midx == idx {
                        z_re[idx] = zk_re * zk_im;
                        z_im[idx] = 0.0;
                    } else {
                        z_re[idx] = c_re;
                        z_im[idx] = c_im;
                        z_re[midx] = c_re;
                        z_im[midx] = -c_im;
                    }
                }
            }
        }
    }
}

/// 实数 IFFT 打包（AVX2 派发）：行频谱 X[0..n]（n=2m）打包为 m 点复数 Z。
/// k=0 需调用方标量处理。
pub fn rifft_pack(re: &mut [f32], im: &mut [f32], m: usize, tw_re: &[f32], tw_im: &[f32]) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 if m >= 8 => unsafe {
            x86::avx2::rifft_pack_avx2(re, im, m, tw_re, tw_im)
        },
        _ => {
            let n = 2 * m;
            for k in 1..m {
                let (xkr, xki) = (re[k], im[k]);
                let (xmr, xmi) = (re[m + k], im[m + k]);
                let e_re = (xkr + xmr) * 0.5;
                let e_im = (xki + xmi) * 0.5;
                let d_re = (xkr - xmr) * 0.5;
                let d_im = (xki - xmi) * 0.5;
                let (twr, twi) = (tw_re[k], tw_im[k]);
                let o_re = d_re * twr - d_im * twi;
                let o_im = d_re * twi + d_im * twr;
                re[k] = e_re - o_im;
                im[k] = e_im + o_re;
            }
            let _ = n;
        }
    }
}

/// 实数 IFFT 解包（AVX2 派发）：m 点复数 z 交错为 n=2m 实数。
pub fn rifft_unpack(re: &mut [f32], im: &mut [f32], m: usize) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 if m >= 8 => unsafe {
            x86::avx2::rifft_unpack_avx2(re, im, m)
        },
        _ => {
            for j in (0..m).rev() {
                re[2 * j] = re[j];
                re[2 * j + 1] = im[j];
            }
        }
    }
}

/// u8 图像行填充到 f32 缓冲（每行拷贝到 `dw` 宽的行首，右侧保持零），AVX2 加速。
pub fn fill_u8_to_f32(src: &[u8], w: usize, h: usize, dst: &mut [f32], dw: usize) {
    debug_assert!(dst.len() >= dw * h);
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => {
            for y in 0..h {
                let srow = &src[y * w..(y + 1) * w];
                let drow = &mut dst[y * dw..y * dw + w];
                unsafe {
                    x86::avx2::u8_to_f32_row(srow, drow);
                }
            }
        }
        x86::Isa::Scalar => {
            for y in 0..h {
                let srow = &src[y * w..(y + 1) * w];
                let drow = &mut dst[y * dw..y * dw + w];
                for (d, s) in drow.iter_mut().zip(srow.iter()) {
                    *d = *s as f32;
                }
            }
        }
    }
}

/// 共轭填充：`dst_re[j] = src_re[j]`，`dst_im[j] = -src_im[j]`（SIMD 加速）。
///
/// 用于 rifft2d 的列方向补全（行 x = conj(行 w-x)）。
pub fn conjugate_fill(
    src_re: &[f32],
    src_im: &[f32],
    dst_re: &mut [f32],
    dst_im: &mut [f32],
) {
    debug_assert_eq!(src_re.len(), src_im.len());
    debug_assert_eq!(src_re.len(), dst_re.len());
    debug_assert_eq!(src_re.len(), dst_im.len());
    let n = src_re.len();
    dst_re.copy_from_slice(src_re);
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::negate_f32(src_im, dst_im);
        },
        x86::Isa::Scalar => {
            for i in 0..n {
                dst_im[i] = -src_im[i];
            }
        }
    }
}

/// AoS 一维 FFT（`Complex` 切片视图，用预计算 plan）。
///
/// `Complex` 为 repr(C) 的 `{re, im}`，与 `2n` 个 f32 布局一致。
pub fn fft1d_complex_plan(data: &mut [crate::imgproc::dft::Complex], plan: &FftPlan) {
    let n = data.len();
    if n <= 1 {
        return;
    }
    // SAFETY: Complex 是 repr(C) 的 {re: f32, im: f32}，2n 个 f32 与 n 个 Complex 布局相同。
    let buf = unsafe {
        std::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut f32, 2 * n)
    };
    fft1d_aos_plan(buf, plan);
}

/// AoS 一维逆 FFT（`Complex` 切片视图，用预计算 plan），带 1/n 缩放。
pub fn ifft1d_complex_plan(data: &mut [crate::imgproc::dft::Complex], plan: &FftPlan) {
    let n = data.len();
    if n <= 1 {
        return;
    }
    // SAFETY: Complex 是 repr(C) 的 {re: f32, im: f32}，2n 个 f32 与 n 个 Complex 布局相同。
    let buf = unsafe {
        std::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut f32, 2 * n)
    };
    ifft1d_aos_plan(buf, plan);
}

/// 缓冲区扩容到 `len` 并清零，多线程池时并行触页（NUMA first-touch）。
///
/// 扩容的新页由并行 worker 首次写入，分布到各 NUMA 节点
/// （串行分配页全落主线程节点，跨节点访问慢 ~25%）。
/// 长度已对时仅清零（复用缓冲区含旧数据）。
pub fn resize_zero_parallel(buf: &mut Vec<f32>, len: usize) {
    if buf.len() == len {
        if rayon::current_num_threads() > 1 {
            use rayon::prelude::*;
            buf.par_chunks_mut(64 * 1024).for_each(|c| c.fill(0.0));
        } else {
            buf.fill(0.0);
        }
        return;
    }
    if buf.capacity() < len {
        buf.reserve_exact(len);
        let spare = buf.spare_capacity_mut();
        if rayon::current_num_threads() > 1 && spare.len() >= 64 * 1024 {
            use rayon::prelude::*;
            spare.par_chunks_mut(64 * 1024).for_each(|c| {
                c.fill(std::mem::MaybeUninit::new(0.0f32));
            });
        } else {
            for v in spare {
                v.write(0.0);
            }
        }
        // SAFETY: spare 已全部初始化为 0.0，长度 <= capacity
        unsafe { buf.set_len(len) };
    } else {
        // 容量够：resize 补零后并行重写（页已存在，纯写）
        buf.resize(len, 0.0);
        if rayon::current_num_threads() > 1 {
            use rayon::prelude::*;
            buf.par_chunks_mut(64 * 1024).for_each(|c| c.fill(0.0));
        }
    }
}

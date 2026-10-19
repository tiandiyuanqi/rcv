//! 离散傅里叶变换（DFT/FFT），用于 matchTemplate 大模板路径的频域互相关。
//!
//! 对齐 OpenCV 5 `modules/core/src/dxt.cpp` 的 `dft`/`mulSpectrums` 语义，
//! 但采用功能正确的 radix-2 迭代 FFT（非 OpenCV 的混合基 FFT）。
//! 归一化方法（TM_SQDIFF_NORMED / TM_CCORR_NORMED / TM_CCOEFF_NORMED）
//! 对微小浮点差异不敏感，PSNR 可保持 > 60dB。

/// 复数（f32）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Complex {
    pub re: f32,
    pub im: f32,
}

impl Complex {
    #[inline]
    pub fn new(re: f32, im: f32) -> Self {
        Complex { re, im }
    }
}

/// 一维 FFT（radix-2 迭代，Cooley-Tukey）。
///
/// `data` 为 `n` 个复数，`n` 必须是 2 的幂。原地变换。
/// 正变换（无缩放）。AVX2 加速（一次 4 蝶形）。
pub fn fft1d(data: &mut [Complex]) {
    let n = data.len();
    assert!(n.is_power_of_two(), "FFT size must be a power of two, got {n}");
    if n <= 1 {
        return;
    }
    crate::simd_kernels::fft::fft1d_complex_plan(data, &crate::simd_kernels::fft::FftPlan::new(n));
}

/// 一维逆 FFT（radix-2 迭代），带 1/n 缩放。
pub fn ifft1d(data: &mut [Complex]) {
    let n = data.len();
    assert!(n.is_power_of_two(), "IFFT size must be a power of two, got {n}");
    if n <= 1 {
        return;
    }
    crate::simd_kernels::fft::ifft1d_complex_plan(data, &crate::simd_kernels::fft::FftPlan::new(n));
}

/// 二维 FFT（行 + 列），原地变换。
///
/// `data` 为 `w*h` 个复数（行优先），`w` 和 `h` 必须是 2 的幂。
/// 旋转因子预计算一次（FftPlan），所有行共享。
pub fn fft2d(data: &mut [Complex], w: usize, h: usize) {
    assert!(w.is_power_of_two() && h.is_power_of_two());
    let plan_w = crate::simd_kernels::fft::FftPlan::new(w.max(2));
    let plan_h = crate::simd_kernels::fft::FftPlan::new(h.max(2));
    // 行变换
    for row in data.chunks_exact_mut(w) {
        fft1d_plan(row, &plan_w);
    }
    // 列变换：转置 → 行变换 → 转置回来（缓存友好，避免跨步访问）
    transpose(data, w, h);
    for row in data.chunks_exact_mut(h) {
        fft1d_plan(row, &plan_h);
    }
    transpose(data, h, w);
}

/// 一维 FFT（AoS，用预计算 plan）。
fn fft1d_plan(data: &mut [Complex], plan: &crate::simd_kernels::fft::FftPlan) {
    crate::simd_kernels::fft::fft1d_complex_plan(data, plan);
}

/// 二维逆 FFT（行 + 列），带 1/(w*h) 缩放。
pub fn ifft2d(data: &mut [Complex], w: usize, h: usize) {
    assert!(w.is_power_of_two() && h.is_power_of_two());
    let plan_w = crate::simd_kernels::fft::FftPlan::new(w.max(2));
    let plan_h = crate::simd_kernels::fft::FftPlan::new(h.max(2));
    // 行变换
    for row in data.chunks_exact_mut(w) {
        ifft1d_plan(row, &plan_w);
    }
    // 列变换：转置 → 行变换 → 转置回来
    transpose(data, w, h);
    for row in data.chunks_exact_mut(h) {
        ifft1d_plan(row, &plan_h);
    }
    transpose(data, h, w);
}

/// 一维逆 FFT（AoS，用预计算 plan）。
fn ifft1d_plan(data: &mut [Complex], plan: &crate::simd_kernels::fft::FftPlan) {
    crate::simd_kernels::fft::ifft1d_complex_plan(data, plan);
}

/// 二维 FFT（SoA 布局，re/im 分离），原地变换。
///
/// `re`/`im` 各 `w*h` 个 f32（行优先），`w` 和 `h` 必须是 2 的幂。
/// 旋转因子预计算一次（FftPlan），所有行共享，避免每行重复计算 cos/sin。
pub fn fft2d_soa(re: &mut [f32], im: &mut [f32], w: usize, h: usize) {
    assert!(w.is_power_of_two() && h.is_power_of_two());
    let plan_w = crate::simd_kernels::fft::FftPlan::new(w.max(2));
    let plan_h = crate::simd_kernels::fft::FftPlan::new(h.max(2));
    // 行变换
    for (rrow, irow) in re.chunks_exact_mut(w).zip(im.chunks_exact_mut(w)) {
        crate::simd_kernels::fft::fft1d_soa_plan(rrow, irow, &plan_w);
    }
    // 列变换：转置 → 行变换 → 转置回来
    transpose_soa(re, im, w, h);
    for (rrow, irow) in re.chunks_exact_mut(h).zip(im.chunks_exact_mut(h)) {
        crate::simd_kernels::fft::fft1d_soa_plan(rrow, irow, &plan_h);
    }
    transpose_soa(re, im, h, w);
}

/// 二维 FFT（SoA 布局），输出转置布局。
///
/// 与 `fft2d_soa` 相同，但跳过最后的转置——输出为转置矩阵
/// （元素 (i,j) 在位置 `j*w+i`）。供 `ccorr_fft` 使用：
/// 后续 `ccorr_unpack`/`rifft2d` 以转置布局处理，省 2 次转置。
///
/// 行间独立，rayon 并行（多线程池时；单线程走串行避免任务分发开销）。
pub fn fft2d_soa_transposed(re: &mut [f32], im: &mut [f32], w: usize, h: usize) {
    assert!(w.is_power_of_two() && h.is_power_of_two());
    let plan_w = crate::simd_kernels::fft::FftPlan::new(w.max(2));
    let plan_h = crate::simd_kernels::fft::FftPlan::new(h.max(2));
    // 行变换（行间独立，并行）
    par_rows_fft(re, im, w, h, &plan_w);
    // 列变换：转置 → 行变换（结果保持转置布局，不再转回）
    transpose_soa(re, im, w, h);
    par_rows_fft(re, im, h, w, &plan_h);
}

/// 行 FFT 并行派发：多线程池用 rayon 大块并行，单线程串行。
///
/// `row_len` 为行长度，`nrows` 为行数。每块 >= MIN_ROWS 行，
/// 避免过订阅（项目经验：rayon fork-join ~33us/次，小任务反而慢）。
fn par_rows_fft(
    re: &mut [f32],
    im: &mut [f32],
    row_len: usize,
    nrows: usize,
    plan: &crate::simd_kernels::fft::FftPlan,
) {
    const MIN_ROWS_PER_BLOCK: usize = 64;
    let nthreads = rayon::current_num_threads();
    if nthreads <= 1 || nrows < MIN_ROWS_PER_BLOCK * 2 {
        // 串行：单线程池或行数太少（并行收益 < 开销）
        for (rrow, irow) in re.chunks_exact_mut(row_len).zip(im.chunks_exact_mut(row_len)) {
            crate::simd_kernels::fft::fft1d_soa_plan(rrow, irow, plan);
        }
        return;
    }
    use rayon::prelude::*;
    // 块数 = min(行数/最小块, 线程数*4)：多块让 rayon 工作窃取平衡负载
    // （64 线程时若只有 64 块，调度不均导致部分线程闲置；×4 后每线程
    // 平均 4 块，快线程多干）。每块仍 >= 64 行，fork-join 开销可忽略。
    let max_blocks = nrows.div_ceil(MIN_ROWS_PER_BLOCK).min(nthreads * 4);
    let rows_per_block = nrows.div_ceil(max_blocks);
    let block_len = rows_per_block * row_len;
    re.par_chunks_mut(block_len)
        .zip(im.par_chunks_mut(block_len))
        .for_each(|(rblk, iblk)| {
            for (rrow, irow) in rblk.chunks_exact_mut(row_len).zip(iblk.chunks_exact_mut(row_len)) {
                crate::simd_kernels::fft::fft1d_soa_plan(rrow, irow, plan);
            }
        });
}

/// 二维逆 FFT（SoA 布局），带 1/(w*h) 缩放。
///
/// 行 IFFT 不缩放，列 IFFT 统一乘 1/(w*h)——省去行方向的缩放 pass
/// （每行 2×4KB 额外读写）。
pub fn ifft2d_soa(re: &mut [f32], im: &mut [f32], w: usize, h: usize) {
    assert!(w.is_power_of_two() && h.is_power_of_two());
    let plan_w = crate::simd_kernels::fft::FftPlan::new(w.max(2));
    let plan_h = crate::simd_kernels::fft::FftPlan::new(h.max(2));
    // 行变换（不缩放）
    for (rrow, irow) in re.chunks_exact_mut(w).zip(im.chunks_exact_mut(w)) {
        crate::simd_kernels::fft::ifft1d_soa_plan_scaled(rrow, irow, &plan_w, 1.0);
    }
    // 列变换：转置 → 行变换（统一缩放）→ 转置回来
    transpose_soa(re, im, w, h);
    let scale = 1.0 / (w as f32 * h as f32);
    for (rrow, irow) in re.chunks_exact_mut(h).zip(im.chunks_exact_mut(h)) {
        crate::simd_kernels::fft::ifft1d_soa_plan_scaled(rrow, irow, &plan_h, scale);
    }
    transpose_soa(re, im, h, w);
}

/// 2D 实数逆 FFT（SoA 布局）。
///
/// 输入：实数信号的 2D 频谱（`re`/`im` 各 `w*h`，行/列均共轭对称），
/// 输出：实数信号写入 `re`（`im` 清零）。带 1/(w*h) 缩放。
///
/// 利用共轭对称性把每个方向的 N 点复数 IFFT 降为 N/2 点：
/// 打包 `Z[k] = E[k] + i·O[k]`，其中
/// `E[k] = (X[k] + conj(X[M-k]))/2`，`O[k] = e^{2πik/N}·(X[k] - conj(X[M-k]))/2`，
/// M 点 IFFT 得 z 后 `x[2n] = Re(z[n])`，`x[2n+1] = Im(z[n])`。
/// k=0 时 X[0] 与 X[M]（Nyquist）均为实数。
pub fn rifft2d_soa(re: &mut [f32], im: &mut [f32], w: usize, h: usize) {
    assert!(w.is_power_of_two() && h.is_power_of_two());
    assert!(w >= 4 && h >= 4, "rifft2d_soa requires >= 4x4");
    let scale = 1.0 / (w as f32 * h as f32);
    let half_w = w / 2;
    // 列方向复数 IFFT：转置后（h×w，原 kx 为行号），只算行 0..=half_w，
    // 其余行 = 共轭（G[x][y] = conj(G[w-x][y])，由 2D 共轭对称性）。
    let plan_h = crate::simd_kernels::fft::FftPlan::new(h);
    transpose_soa(re, im, w, h);
    for row in 0..=half_w {
        let base = row * h;
        crate::simd_kernels::fft::ifft1d_soa_plan_scaled(
            &mut re[base..base + h],
            &mut im[base..base + h],
            &plan_h,
            1.0,
        );
    }
    // 补全：行 x = conj(行 w-x)（x = half_w+1..w-1）
    // 注意：x > half_w 时 w-x < half_w < x，故 src < dst，可用 split_at_mut 分离。
    for x in (half_w + 1)..w {
        let src = (w - x) * h;
        let dst = x * h;
        let (first_re, second_re) = re.split_at_mut(dst);
        let (first_im, second_im) = im.split_at_mut(dst);
        crate::simd_kernels::fft::conjugate_fill(
            &first_re[src..src + h],
            &first_im[src..src + h],
            &mut second_re[0..h],
            &mut second_im[0..h],
        );
    }
    transpose_soa(re, im, h, w);
    // 行方向实数 IFFT（列 IFFT 后每行满足共轭对称：g(y,x) = conj(g(y,w-x))）
    let plan_w = crate::simd_kernels::fft::FftPlan::new(w / 2);
    // 预计算 e^{2πik/w}（k=1..w/2-1），避免每行重复算 cos/sin
    let mut tw_re = vec![0.0f32; w / 2];
    let mut tw_im = vec![0.0f32; w / 2];
    for k in 1..w / 2 {
        let ang = 2.0 * std::f32::consts::PI * (k as f32) / (w as f32);
        tw_re[k] = ang.cos();
        tw_im[k] = ang.sin();
    }
    for (rrow, irow) in re.chunks_exact_mut(w).zip(im.chunks_exact_mut(w)) {
        rifft1d_row(rrow, irow, &plan_w, scale, &tw_re, &tw_im);
    }
}

/// 2D 实数逆 FFT（SoA 布局），输入为转置布局。
///
/// 与 `rifft2d_soa` 相同，但输入频谱已是转置布局（元素 (kx, ky) 在
/// 位置 `kx*h + ky`，来自 `fft2d_soa_transposed`），跳过第一次转置。
/// 输出为正常布局（实数信号，行长度 w）。
///
/// 列 IFFT（行 0..=half_w）与行实数 IFFT 行间独立，rayon 并行。
pub fn rifft2d_soa_transposed(re: &mut [f32], im: &mut [f32], w: usize, h: usize) {
    assert!(w.is_power_of_two() && h.is_power_of_two());
    assert!(w >= 4 && h >= 4, "rifft2d_soa requires >= 4x4");
    let scale = 1.0 / (w as f32 * h as f32);
    let half_w = w / 2;
    // 输入已是转置布局（行 kx，长度 h）：直接对行 0..=half_w 做 ky 方向 IFFT。
    // 行间独立，并行（只算前 half_w+1 行，其余由共轭补全）。
    {
        let plan_h = crate::simd_kernels::fft::FftPlan::new(h);
        let nrows = half_w + 1;
        const MIN_ROWS_PER_BLOCK: usize = 64;
        let nthreads = rayon::current_num_threads();
        if nthreads <= 1 || nrows < MIN_ROWS_PER_BLOCK * 2 {
            for row in 0..nrows {
                let base = row * h;
                crate::simd_kernels::fft::ifft1d_soa_plan_scaled(
                    &mut re[base..base + h],
                    &mut im[base..base + h],
                    &plan_h,
                    1.0,
                );
            }
        } else {
            use rayon::prelude::*;
            let max_blocks = nrows.div_ceil(MIN_ROWS_PER_BLOCK).min(nthreads * 4);
            let rows_per_block = nrows.div_ceil(max_blocks);
            let block_len = rows_per_block * h;
            re.par_chunks_mut(block_len)
                .zip(im.par_chunks_mut(block_len))
                .take(nrows.div_ceil(rows_per_block))
                .for_each(|(rblk, iblk)| {
                    let rows = rblk.len() / h;
                    for row in 0..rows {
                        let base = row * h;
                        crate::simd_kernels::fft::ifft1d_soa_plan_scaled(
                            &mut rblk[base..base + h],
                            &mut iblk[base..base + h],
                            &plan_h,
                            1.0,
                        );
                    }
                });
        }
    }
    // 补全：行 x = conj(行 w-x)（x = half_w+1..w-1）
    // 注意：x > half_w 时 w-x < half_w < x，故 src < dst，可用 split_at_mut 分离。
    for x in (half_w + 1)..w {
        let src = (w - x) * h;
        let dst = x * h;
        let (first_re, second_re) = re.split_at_mut(dst);
        let (first_im, second_im) = im.split_at_mut(dst);
        crate::simd_kernels::fft::conjugate_fill(
            &first_re[src..src + h],
            &first_im[src..src + h],
            &mut second_re[0..h],
            &mut second_im[0..h],
        );
    }
    // 转置回正常布局（行 ky，长度 w），行方向实数 IFFT（行间独立，并行）
    transpose_soa(re, im, h, w);
    let plan_w = crate::simd_kernels::fft::FftPlan::new(w / 2);
    let mut tw_re = vec![0.0f32; w / 2];
    let mut tw_im = vec![0.0f32; w / 2];
    for k in 1..w / 2 {
        let ang = 2.0 * std::f32::consts::PI * (k as f32) / (w as f32);
        tw_re[k] = ang.cos();
        tw_im[k] = ang.sin();
    }
    {
        const MIN_ROWS_PER_BLOCK: usize = 64;
        let nthreads = rayon::current_num_threads();
        if nthreads <= 1 || h < MIN_ROWS_PER_BLOCK * 2 {
            for (rrow, irow) in re.chunks_exact_mut(w).zip(im.chunks_exact_mut(w)) {
                rifft1d_row(rrow, irow, &plan_w, scale, &tw_re, &tw_im);
            }
        } else {
            use rayon::prelude::*;
            let max_blocks = h.div_ceil(MIN_ROWS_PER_BLOCK).min(nthreads * 4);
            let rows_per_block = h.div_ceil(max_blocks);
            let block_len = rows_per_block * w;
            re.par_chunks_mut(block_len)
                .zip(im.par_chunks_mut(block_len))
                .for_each(|(rblk, iblk)| {
                    for (rrow, irow) in
                        rblk.chunks_exact_mut(w).zip(iblk.chunks_exact_mut(w))
                    {
                        rifft1d_row(rrow, irow, &plan_w, scale, &tw_re, &tw_im);
                    }
                });
        }
    }
}

/// 单行实数 IFFT（原地，`re`/`im` 为该行频谱，长度 n=2m）。
///
/// 打包成 m 点复数 IFFT 后解包：实部写 `re` 的偶数位，虚部写奇数位。
/// 打包/解包用 AVX2 内核，k=0 边界标量处理。
fn rifft1d_row(
    re: &mut [f32],
    im: &mut [f32],
    plan: &crate::simd_kernels::fft::FftPlan,
    scale: f32,
    tw_re: &[f32],
    tw_im: &[f32],
) {
    let n = re.len();
    debug_assert_eq!(im.len(), n);
    let m = n / 2;
    // k=0：X[0] 与 X[m]（Nyquist）均实数，tw[0]=1
    let x0 = re[0];
    let xm = re[m];
    // 打包 k=1..m-1（SIMD）
    crate::simd_kernels::fft::rifft_pack(re, im, m, tw_re, tw_im);
    // Z[0] = (x0+xm)/2 + i*(x0-xm)/2
    re[0] = (x0 + xm) * 0.5;
    im[0] = (x0 - xm) * 0.5;
    // m 点复数 IFFT。缩放换算：解包(m点IFFT带s缩放) = 2s·(n点IFFT带1/n缩放)，
    // 要等价 n 点 IFFT × scale，需 s = 2·scale。
    crate::simd_kernels::fft::ifft1d_soa_plan_scaled(&mut re[..m], &mut im[..m], plan, scale * 2.0);
    // 解包：x[2j] = Re(z[j]), x[2j+1] = Im(z[j])（SIMD，从后往前）
    crate::simd_kernels::fft::rifft_unpack(re, im, m);
}

/// 转置 `w*h` 复数矩阵（SoA 布局，re/im 分离），转置后为 `h*w`。
fn transpose_soa(re: &mut [f32], im: &mut [f32], w: usize, h: usize) {
    if w == h {
        transpose_square_soa(re, im, w);
    } else {
        let mut tre = vec![0.0f32; re.len()];
        let mut tim = vec![0.0f32; im.len()];
        for i in 0..h {
            for j in 0..w {
                tre[j * h + i] = re[i * w + j];
                tim[j * h + i] = im[i * w + j];
            }
        }
        re.copy_from_slice(&tre);
        im.copy_from_slice(&tim);
    }
}

/// 方阵原地分块转置（SoA 布局，8x8 块）。
fn transpose_square_soa(re: &mut [f32], im: &mut [f32], n: usize) {
    crate::simd_kernels::fft::transpose_square_soa(re, im, n);
}

/// 转置 `w*h` 复数矩阵（行优先），转置后为 `h*w`。
///
/// 元素 `(i,j)` 原位置 `i*w+j`，转置后位置 `j*h+i`。
/// 方阵用 8x8 分块原地转置（缓存友好）；非方阵用临时缓冲。
fn transpose(data: &mut [Complex], w: usize, h: usize) {
    if w == h {
        transpose_square(data, w);
    } else {
        let mut tmp = vec![Complex::default(); data.len()];
        for i in 0..h {
            for j in 0..w {
                tmp[j * h + i] = data[i * w + j];
            }
        }
        data.copy_from_slice(&tmp);
    }
}

/// 方阵原地分块转置（8x8 块）。
fn transpose_square(data: &mut [Complex], n: usize) {
    const BLOCK: usize = 8;
    let nb = n.div_ceil(BLOCK);
    for by in 0..nb {
        for bx in 0..nb {
            let y0 = by * BLOCK;
            let x0 = bx * BLOCK;
            let y1 = (y0 + BLOCK).min(n);
            let x1 = (x0 + BLOCK).min(n);
            if by == bx {
                // 对角块：块内转置
                for i in y0..y1 {
                    for j in x0..x1 {
                        if i < j {
                            data.swap(i * n + j, j * n + i);
                        }
                    }
                }
            } else if by < bx {
                // 非对角块对：交换对应元素
                for i in y0..y1 {
                    for j in x0..x1 {
                        data.swap(i * n + j, j * n + i);
                    }
                }
            }
        }
    }
}

/// 频域复数乘法（对齐 OpenCV `mulSpectrums`，`conjB=true`）。
///
/// `a` 和 `b` 为 `n` 个复数，结果写入 `a`。`conj_b` 为 true 时对 `b` 取共轭
/// （用于互相关而非卷积）。
pub fn mul_spectrums(a: &mut [Complex], b: &[Complex], conj_b: bool) {
    for (x, y) in a.iter_mut().zip(b.iter()) {
        let (br, bi) = if conj_b { (y.re, -y.im) } else { (y.re, y.im) };
        let re = x.re * br - x.im * bi;
        let im = x.re * bi + x.im * br;
        x.re = re;
        x.im = im;
    }
}

/// 计算最优 DFT 尺寸（对齐 OpenCV `getOptimalDFTSize`）。
///
/// 返回 `>= size0` 的最小 5-smooth 数（2^a * 3^b * 5^c）。
pub fn get_optimal_dft_size(size0: usize) -> usize {
    // 枚举所有 5-smooth 数（2^a * 3^b * 5^c），找到 >= size0 的最小值。
    // 上限：size0 的 2 倍以内必有 5-smooth 数（2*size0 本身是 2-smooth）。
    let limit = size0.saturating_mul(2).max(1);
    let mut best = limit;
    let mut a = 0usize;
    loop {
        let p2 = 1usize << a;
        if p2 > limit {
            break;
        }
        let mut b = 0usize;
        let mut p3 = 1usize;
        loop {
            let base = p2 * p3;
            if base > limit {
                break;
            }
            let mut c = 0usize;
            let mut p5 = 1usize;
            loop {
                let v = base * p5;
                if v > limit {
                    break;
                }
                if v >= size0 && v < best {
                    best = v;
                }
                c += 1;
                p5 = p5.saturating_mul(5);
                if c > 30 {
                    break;
                }
            }
            b += 1;
            p3 = p3.saturating_mul(3);
            if b > 30 {
                break;
            }
        }
        a += 1;
        if a > 30 {
            break;
        }
    }
    best
}

/// 用 FFT 计算二维互相关（CCORR）。
///
/// `src` 为 `w*h` 灰度像素，`templ` 为 `tw*th` 模板。输出 `corr` 为
/// `(w-tw+1)*(h-th+1)` 的 f32 互相关值。
///
/// 算法：将 src 和 templ 补零到 `(dw, dh)`（2 的幂），各自 FFT，
/// 频域相乘（模板共轭），逆 FFT，取左上 `(w-tw+1, h-th+1)` 区域。
pub fn ccorr_fft(
    src: &[u8],
    w: usize,
    h: usize,
    templ: &[u8],
    tw: usize,
    th: usize,
    corr: &mut [f32],
) {
    let (rw, rh) = (w - tw + 1, h - th + 1);
    // DFT 尺寸：>= (rw + tw - 1, rh + th - 1) 的 2 的幂
    let dw = (rw + tw - 1).next_power_of_two();
    let dh = (rh + th - 1).next_power_of_two();

    // RFFT 打包技巧：z = img + i*tpl，一次 FFT 同时得到两者的频谱。
    // Z = FFT(z)，则 IMG[k] = (Z[k] + conj(Z[N-k]))/2，TPL[k] = (Z[k] - conj(Z[N-k]))/(2i)。
    // 将 2 次正变换合并为 1 次，节省约 1/3 的 FFT 总耗时。
    // SoA 布局：re/im 分离，蝶形无 shuffle，转置用 AVX2 8x8 块内核。
    // 缓冲区用 thread_local 复用：避免每次 8MB 分配的页错误（~2-4ms）。
    thread_local! {
        static BUF: std::cell::RefCell<(Vec<f32>, Vec<f32>)> =
            const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
    }
    let (mut z_re, mut z_im) = BUF.with(|b| std::mem::take(&mut *b.borrow_mut()));
    // 复用缓冲区需清零（fill 只写图像区域，padding 依赖零）。
    // 扩容时并行触页（first-touch：新页分布到各 NUMA 节点，
    // 串行分配页全落主线程节点，跨节点访问慢 ~25%）。
    crate::simd_kernels::fft::resize_zero_parallel(&mut z_re, dw * dh);
    crate::simd_kernels::fft::resize_zero_parallel(&mut z_im, dw * dh);
    // u8 → f32 填充（AVX2 一次 8 像素）
    crate::simd_kernels::fft::fill_u8_to_f32(src, w, h, &mut z_re, dw);
    crate::simd_kernels::fft::fill_u8_to_f32(templ, tw, th, &mut z_im, dw);

    // 一次 FFT 得到打包频谱（输出转置布局，省去 fft2d 结尾的转置）
    fft2d_soa_transposed(&mut z_re, &mut z_im, dw, dh);

    // 解包 + 频域互相关（AVX2，成对处理对称点）：
    //   corr_spec[k] = Im(Zk·Zm)/2 + i(|Zk|²-|Zm|²)/4，Zm = Z[N-k]
    // 自对称点（k = 0 或 Nyquist）：corr = Re(Zk)·Im(Zk)（实数）。
    // 输入为转置布局（元素 (kx,ky) 在位置 kx*dh+ky），交换 dw/dh 参数即可。
    crate::simd_kernels::fft::ccorr_unpack(&mut z_re, &mut z_im, dh, dw);

    // 逆 FFT（实数：corr 是实数信号，频谱共轭对称，行方向减半）。
    // 输入已是转置布局，跳过 rifft2d 的第一次转置。
    rifft2d_soa_transposed(&mut z_re, &mut z_im, dw, dh);

    // 取左上 (rw, rh) 区域（行独立，多线程池时并行）
    {
        let nthreads = rayon::current_num_threads();
        if nthreads > 1 && rh >= 128 {
            use rayon::prelude::*;
            corr.par_chunks_exact_mut(rw).enumerate().for_each(|(y, out)| {
                let src = &z_re[y * dw..y * dw + rw];
                out.copy_from_slice(src);
            });
        } else {
            for y in 0..rh {
                corr[y * rw..y * rw + rw].copy_from_slice(&z_re[y * dw..y * dw + rw]);
            }
        }
    }
    // 归还缓冲区（容量保留，下次调用复用）
    BUF.with(|b| *b.borrow_mut() = (z_re, z_im));
}

/// 频域复数乘法（SoA 布局，`conj_b=true`）。
///
/// `a` 和 `b` 为 `n` 个复数（re/im 分离），结果写入 `a`。对 `b` 取共轭
/// （用于互相关而非卷积）。
fn mul_spectrums_soa(
    a_re: &mut [f32],
    a_im: &mut [f32],
    b_re: &[f32],
    b_im: &[f32],
) {
    for i in 0..a_re.len() {
        let (br, bi) = (b_re[i], -b_im[i]);
        let re = a_re[i] * br - a_im[i] * bi;
        let im = a_re[i] * bi + a_im[i] * br;
        a_re[i] = re;
        a_im[i] = im;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一维 FFT 与直接 DFT 对比。
    #[test]
    fn fft1d_matches_direct() {
        for n in [2, 4, 8, 16, 32, 64] {
            let data: Vec<Complex> = (0..n)
                .map(|i| Complex::new((i as f32 * 0.7).sin(), (i as f32 * 0.3).cos()))
                .collect();
            let mut fft = data.clone();
            fft1d(&mut fft);
            // 直接 DFT
            let mut direct = vec![Complex::default(); n];
            for k in 0..n {
                let mut sum = Complex::new(0.0, 0.0);
                for j in 0..n {
                    let ang = -2.0 * std::f32::consts::PI * (k * j) as f32 / n as f32;
                    let w = Complex::new(ang.cos(), ang.sin());
                    sum.re += data[j].re * w.re - data[j].im * w.im;
                    sum.im += data[j].re * w.im + data[j].im * w.re;
                }
                direct[k] = sum;
            }
            for k in 0..n {
                assert!(
                    (fft[k].re - direct[k].re).abs() < 1e-3,
                    "n={n} k={k} re: fft={} direct={}",
                    fft[k].re,
                    direct[k].re
                );
                assert!(
                    (fft[k].im - direct[k].im).abs() < 1e-3,
                    "n={n} k={k} im: fft={} direct={}",
                    fft[k].im,
                    direct[k].im
                );
            }
        }
    }

    /// 逆 FFT 恢复原信号。
    #[test]
    fn ifft1d_roundtrip() {
        for n in [2, 4, 8, 16, 32] {
            let data: Vec<Complex> = (0..n)
                .map(|i| Complex::new((i as f32 * 0.5).cos(), (i as f32 * 0.2).sin()))
                .collect();
            let mut buf = data.clone();
            fft1d(&mut buf);
            ifft1d(&mut buf);
            for i in 0..n {
                assert!(
                    (buf[i].re - data[i].re).abs() < 1e-4,
                    "n={n} i={i} re: {} vs {}",
                    buf[i].re,
                    data[i].re
                );
                assert!(
                    (buf[i].im - data[i].im).abs() < 1e-4,
                    "n={n} i={i} im: {} vs {}",
                    buf[i].im,
                    data[i].im
                );
            }
        }
    }

    /// 二维 FFT 与一维行/列分解对比。
    #[test]
    fn fft2d_matches_separable() {
        let (w, h) = (8, 4);
        let data: Vec<Complex> = (0..w * h)
            .map(|i| Complex::new((i as f32 * 0.3).sin(), (i as f32 * 0.1).cos()))
            .collect();
        let mut fft = data.clone();
        fft2d(&mut fft, w, h);
        // 直接二维 DFT
        let mut direct = vec![Complex::default(); w * h];
        for ky in 0..h {
            for kx in 0..w {
                let mut sum = Complex::new(0.0, 0.0);
                for jy in 0..h {
                    for jx in 0..w {
                        let ang = -2.0 * std::f32::consts::PI
                            * ((kx * jx) as f32 / w as f32 + (ky * jy) as f32 / h as f32);
                        let wv = Complex::new(ang.cos(), ang.sin());
                        let v = data[jy * w + jx];
                        sum.re += v.re * wv.re - v.im * wv.im;
                        sum.im += v.re * wv.im + v.im * wv.re;
                    }
                }
                direct[ky * w + kx] = sum;
            }
        }
        for i in 0..w * h {
            assert!((fft[i].re - direct[i].re).abs() < 1e-2);
            assert!((fft[i].im - direct[i].im).abs() < 1e-2);
        }
    }

    /// 实数 IFFT 与复数 IFFT 一致性（共轭对称频谱输入）。
    #[test]
    fn rifft2d_matches_ifft2d() {
        for (w, h) in [(16usize, 16usize), (32, 64), (64, 32), (128, 128)] {
            // 构造实数信号
            let mut sig = vec![0.0f32; w * h];
            for y in 0..h {
                for x in 0..w {
                    sig[y * w + x] =
                        ((x as f32 * 0.3).sin() * (y as f32 * 0.7).cos() * 3.0 + 2.0);
                }
            }
            // 正变换得频谱
            let mut re = sig.clone();
            let mut im = vec![0.0f32; w * h];
            fft2d_soa(&mut re, &mut im, w, h);
            // 复数 IFFT
            let mut re_c = re.clone();
            let mut im_c = im.clone();
            ifft2d_soa(&mut re_c, &mut im_c, w, h);
            // 实数 IFFT
            let mut re_r = re.clone();
            let mut im_r = im.clone();
            rifft2d_soa(&mut re_r, &mut im_r, w, h);
            let mut maxerr = 0.0f32;
            for i in 0..w * h {
                maxerr = maxerr.max((re_r[i] - re_c[i]).abs());
            }
            assert!(maxerr < 1e-3, "w={w} h={h} maxerr={maxerr}");
        }
    }

    /// FFT 互相关与暴力卷积对比（相对误差）。
    #[test]
    fn ccorr_fft_matches_bruteforce() {
        let (w, h) = (64, 64);
        let (tw, th) = (32, 32);
        // 随机图像
        let mut src = vec![0u8; w * h];
        let mut templ = vec![0u8; tw * th];
        let mut seed = 12345u32;
        for v in src.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (seed >> 24) as u8;
        }
        for v in templ.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (seed >> 24) as u8;
        }
        let (rw, rh) = (w - tw + 1, h - th + 1);
        // 暴力
        let mut brute = vec![0.0f32; rw * rh];
        for y in 0..rh {
            for x in 0..rw {
                let mut s = 0.0f32;
                for ty in 0..th {
                    for tx in 0..tw {
                        s += templ[ty * tw + tx] as f32 * src[(y + ty) * w + (x + tx)] as f32;
                    }
                }
                brute[y * rw + x] = s;
            }
        }
        // FFT
        let mut fft = vec![0.0f32; rw * rh];
        ccorr_fft(&src, w, h, &templ, tw, th, &mut fft);
        // 对比：相对误差
        for i in 0..rw * rh {
            let denom = brute[i].abs().max(1.0);
            let rel = (fft[i] - brute[i]).abs() / denom;
            assert!(
                rel < 1e-3,
                "i={i} brute={} fft={} rel={}",
                brute[i],
                fft[i],
                rel
            );
        }
    }

    /// get_optimal_dft_size 返回 >= 输入的最小 5-smooth 数。
    #[test]
    fn optimal_dft_size() {
        assert_eq!(get_optimal_dft_size(1), 1);
        assert_eq!(get_optimal_dft_size(2), 2);
        assert_eq!(get_optimal_dft_size(3), 3);
        assert_eq!(get_optimal_dft_size(4), 4);
        assert_eq!(get_optimal_dft_size(5), 5);
        assert_eq!(get_optimal_dft_size(6), 6);
        assert_eq!(get_optimal_dft_size(7), 8);
        assert_eq!(get_optimal_dft_size(8), 8);
        assert_eq!(get_optimal_dft_size(9), 9);
        assert_eq!(get_optimal_dft_size(10), 10);
        assert_eq!(get_optimal_dft_size(11), 12);
        assert_eq!(get_optimal_dft_size(12), 12);
        assert_eq!(get_optimal_dft_size(13), 15);
        assert_eq!(get_optimal_dft_size(15), 15);
        assert_eq!(get_optimal_dft_size(16), 16);
        assert_eq!(get_optimal_dft_size(17), 18);
        assert_eq!(get_optimal_dft_size(31), 32);
        assert_eq!(get_optimal_dft_size(32), 32);
        assert_eq!(get_optimal_dft_size(33), 36);
        assert_eq!(get_optimal_dft_size(63), 64);
        assert_eq!(get_optimal_dft_size(64), 64);
        assert_eq!(get_optimal_dft_size(65), 72);
        assert_eq!(get_optimal_dft_size(100), 100);
        assert_eq!(get_optimal_dft_size(101), 108);
        assert_eq!(get_optimal_dft_size(255), 256);
        assert_eq!(get_optimal_dft_size(256), 256);
        assert_eq!(get_optimal_dft_size(257), 270);
    }


}




















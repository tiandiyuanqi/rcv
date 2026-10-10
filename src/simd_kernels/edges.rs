//! Canny 边缘检测内核（标量基准，对齐 canny.cpp）。
//!
//! SIMD 版本在 `x86/avx2.rs` / `x86/avx512.rs`，本模块是正确性基准与
//! 标量 fallback。数据流：Sobel 16S 梯度（复用 gradient 模块）→
//! 幅度（L1/L2）→ NMS + 双阈值（map: 0=候选 1=非边缘 2=边缘）→
//! 滞后洪泛 → finalPass。

/// 幅度行计算（i16 dx/dy → i32 mag，L1 或 L2）。
///
/// L1: `|dx| + |dy|`；L2: `dx² + dy²`（阈值已在外层平方）。
pub fn mag_row(dx: &[i16], dy: &[i16], mag: &mut [i32], n: usize, l2: bool) {
    if l2 {
        for j in 0..n {
            let x = dx[j] as i32;
            let y = dy[j] as i32;
            mag[j] = x * x + y * y;
        }
    } else {
        for j in 0..n {
            mag[j] = dx[j].wrapping_abs() as i32 + dy[j].wrapping_abs() as i32;
        }
    }
}

/// NMS + 双阈值单行（对齐 canny.cpp:594-638 标量段）。
///
/// `mag_p`/`mag_a`/`mag_n` 为上/当/下幅度行（含 -1 与 w 位置的 0 哨兵），
/// `dx`/`dy` 为当前行梯度，`pmap` 为 map 行（含首尾 1 哨兵，长度 ≥ w+2，
/// 有效区 [1, w]）。候选像素（m > low）按 TG22 定点角度做 NMS，
/// 通过且 m > high 置 2 并入栈，否则置 0；未通过 NMS 置 1。
///
/// 返回入栈的 map 偏移（相对 pmap 起点的列索引）。
#[allow(clippy::too_many_arguments)]
pub fn nms_row(
    mag_p: &[i32],
    mag_a: &[i32],
    mag_n: &[i32],
    dx: &[i16],
    dy: &[i16],
    pmap: &mut [u8],
    w: usize,
    low: i32,
    high: i32,
    stack: &mut Vec<usize>,
) {
    const TG22: i32 = 13573;
    // pmap[0] 与 pmap[w+1] 已由调用方置 1
    for j in 0..w {
        let m = mag_a[j + 1];
        if m > low {
            let xs = dx[j];
            let ys = dy[j];
            let x = xs.wrapping_abs() as i32;
            let y = (ys.wrapping_abs() as i32) << 15;
            let tg22x = x * TG22;
            if y < tg22x {
                // 水平边缘：左右邻
                if m > mag_a[j] && m >= mag_a[j + 2] {
                    if m > high {
                        pmap[j + 1] = 2;
                        stack.push(j + 1);
                    } else {
                        pmap[j + 1] = 0;
                    }
                    continue;
                }
            } else {
                let tg67x = tg22x + (x << 16);
                if y > tg67x {
                    // 垂直边缘：上下邻
                    if m > mag_p[j + 1] && m >= mag_n[j + 1] {
                        if m > high {
                            pmap[j + 1] = 2;
                            stack.push(j + 1);
                        } else {
                            pmap[j + 1] = 0;
                        }
                        continue;
                    }
                } else {
                    // 对角边缘
                    let s: i32 = if (xs ^ ys) < 0 { -1 } else { 1 };
                    let sp = (j as i32 + 1 - s) as usize;
                    let sn = (j as i32 + 1 + s) as usize;
                    if m > mag_p[sp] && m > mag_n[sn] {
                        if m > high {
                            pmap[j + 1] = 2;
                            stack.push(j + 1);
                        } else {
                            pmap[j + 1] = 0;
                        }
                        continue;
                    }
                }
            }
        }
        pmap[j + 1] = 1;
    }
}

/// 滞后阈值洪泛（对齐 canny.cpp:838-851）。
///
/// map 为 (h+2) × mapstep 的 0/1/2 图，栈元素为 map 内的线性偏移。
/// 弹出像素的 8 邻域中 map==0 的置 2 并入栈。
pub fn hysteresis(map: &mut [u8], mapstep: usize, stack: &mut Vec<usize>) {
    while let Some(off) = stack.pop() {
        let neighbors = [
            off.wrapping_sub(mapstep + 1),
            off.wrapping_sub(mapstep),
            off.wrapping_sub(mapstep - 1),
            off - 1,
            off + 1,
            off + mapstep - 1,
            off + mapstep,
            off + mapstep + 1,
        ];
        for nb in neighbors {
            if map[nb] == 0 {
                map[nb] = 2;
                stack.push(nb);
            }
        }
    }
}

/// 滞后阈值洪泛（裸指针版，无 bounds check）。
///
/// 栈元素为 map 内偏移；调用方保证所有偏移及其 8 邻域在 map 范围内
///（哨兵行/列保证邻居不越界：栈元素列 ∈ [1, w]，邻居列 ∈ [0, w+1]，
/// 行 ∈ [1, h]，邻居行 ∈ [0, h+1]）。
///
/// # Safety
/// 调用方契约（"（调用方契约）"后缀不被 clippy 识别，改标准格式）：
/// - 栈内所有偏移 ∈ [mapstep+1, (h+1)*mapstep)（有效像素区）
/// - map 长度 ≥ (h+2)*mapstep
pub unsafe fn hysteresis_unchecked(map: *mut u8, mapstep: usize, stack: &mut Vec<usize>) {
    // SAFETY: 调用方契约保证偏移与邻居不越界
    unsafe {
        while let Some(off) = stack.pop() {
            let m = map.add(off);
            let nbs = [
                m.sub(mapstep + 1),
                m.sub(mapstep),
                m.sub(mapstep - 1),
                m.sub(1),
                m.add(1),
                m.add(mapstep - 1),
                m.add(mapstep),
                m.add(mapstep + 1),
            ];
            for nb in nbs {
                if *nb == 0 {
                    *nb = 2;
                    let nb_off = nb.offset_from(map) as usize;
                    stack.push(nb_off);
                }
            }
        }
    }
}

/// finalPass 单行（对齐 canny.cpp:747-750）：`dst = (map==2) ? 255 : 0`。
pub fn final_pass_row(pmap: &[u8], dst: &mut [u8], w: usize) {
    for j in 0..w {
        dst[j] = if pmap[j + 1] == 2 { 255 } else { 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mag_row_l1_l2() {
        let dx = [3i16, -4, 0];
        let dy = [4i16, 3, -1];
        let mut m = vec![0i32; 3];
        mag_row(&dx, &dy, &mut m, 3, false);
        assert_eq!(m, vec![7, 7, 1]);
        mag_row(&dx, &dy, &mut m, 3, true);
        assert_eq!(m, vec![25, 25, 1]);
    }

    #[test]
    fn nms_horizontal_edge() {
        // 水平梯度（垂直边缘）：dx 大 dy 小 → y < tg22x → 左右比较
        // mag: [0, 5, 10, 5, 0]（含哨兵），dx=[1,1,1], dy=[0,0,0]
        let mag_p = vec![0i32; 5];
        let mag_a = vec![0, 5, 10, 5, 0];
        let mag_n = vec![0i32; 5];
        let dx = [1i16, 1, 1];
        let dy = [0i16, 0, 0];
        let mut pmap = vec![1u8; 5];
        let mut stack = Vec::new();
        nms_row(&mag_p, &mag_a, &mag_n, &dx, &dy, &mut pmap, 3, 1, 5, &mut stack);
        // 中间像素 m=10 > high=5 → 2 入栈；两侧 m=5 不 > low=1 但 NMS 失败（m > 左邻? 5>0 ✓, m>=右邻? 5>=10 ✗）→ 1
        assert_eq!(pmap, vec![1, 1, 2, 1, 1]);
        assert_eq!(stack, vec![2]);
    }

    #[test]
    fn nms_vertical_edge() {
        // 垂直梯度（水平边缘）：dy 大 dx 小 → y > tg67x → 上下比较
        let mag_p = vec![0i32, 3, 3, 3, 0];
        let mag_a = vec![0, 3, 10, 3, 0];
        let mag_n = vec![0, 3, 3, 3, 0];
        let dx = [0i16, 0, 0];
        let dy = [1i16, 1, 1];
        let mut pmap = vec![1u8; 5];
        let mut stack = Vec::new();
        nms_row(&mag_p, &mag_a, &mag_n, &dx, &dy, &mut pmap, 3, 1, 5, &mut stack);
        assert_eq!(pmap, vec![1, 1, 2, 1, 1]);
        assert_eq!(stack, vec![2]);
    }

    #[test]
    fn nms_diagonal_edge() {
        // 对角：xs=1, ys=1 → s=1，比较 mag_p[j] 与 mag_n[j+2]
        let mag_p = vec![0i32, 0, 0, 9, 0];
        let mag_a = vec![0, 0, 10, 0, 0];
        let mag_n = vec![0, 0, 0, 0, 9];
        let dx = [1i16, 1, 1];
        let dy = [1i16, 1, 1];
        let mut pmap = vec![1u8; 5];
        let mut stack = Vec::new();
        nms_row(&mag_p, &mag_a, &mag_n, &dx, &dy, &mut pmap, 3, 1, 5, &mut stack);
        // j=1: m=10, x=1<<0? x=1, y=1<<15=32768, tg22x=13573 → y >= tg22x, tg67x=13573+65536=79109 → y < tg67x → 对角
        // s=1: m > mag_p[1]=0 ✓ && m > mag_n[3]=0 ✓ → 2
        assert_eq!(pmap, vec![1, 1, 2, 1, 1]);
    }

    #[test]
    fn hysteresis_flood() {
        // 3x3 map（含边框行），中心 2，右邻 0 → 洪泛
        let mapstep = 5usize;
        let mut map = vec![
            1u8, 1, 1, 1, 1, //
            1, 2, 0, 1, 1, //
            1, 1, 1, 1, 1, //
        ];
        let mut stack = vec![6usize]; // 行 1 列 1
        hysteresis(&mut map, mapstep, &mut stack);
        assert_eq!(map[mapstep + 2], 2); // 右邻被洪泛
        assert!(stack.is_empty());
    }

    #[test]
    fn final_pass() {
        let pmap = [1u8, 2, 0, 1];
        let mut dst = [0u8; 2];
        final_pass_row(&pmap, &mut dst, 2);
        assert_eq!(dst, [255, 0]);
    }
}

#[cfg(test)]
mod isa_cross_tests {
    use super::*;

    fn has_avx2() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }
    fn has_avx512() -> bool {
        std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl")
    }

    fn rand_i16(n: usize, seed: u64) -> Vec<i16> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 33) as i32 % 2000 - 1000) as i16
            })
            .collect()
    }

    /// 三路径交叉验证：幅度行。
    #[test]
    fn canny_mag_cross_isa() {
        for n in [1usize, 5, 16, 17, 32, 33, 64, 100] {
            let dx = rand_i16(n, n as u64 + 1);
            let dy = rand_i16(n, n as u64 + 2);
            for l2 in [true, false] {
                let mut ref_m = vec![0i32; n];
                mag_row(&dx, &dy, &mut ref_m, n, l2);
                if has_avx2() {
                    let mut m = vec![0i32; n];
                    // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                    unsafe { crate::simd_kernels::x86::avx2::canny_mag_row(&dx, &dy, &mut m, n, l2) };
                    assert_eq!(m, ref_m, "avx2 mag n={n} l2={l2}");
                }
                if has_avx512() {
                    let mut m = vec![0i32; n];
                    // SAFETY: 测试环境已探测 AVX512；切片长度与内核契约一致
                    unsafe {
                        crate::simd_kernels::x86::avx512::canny_mag_row(&dx, &dy, &mut m, n, l2)
                    };
                    assert_eq!(m, ref_m, "avx512 mag n={n} l2={l2}");
                }
            }
        }
    }

    /// 三路径交叉验证：low 掩码批。
    #[test]
    fn canny_low_mask_cross_isa() {
        let n = 32;
        let mag: Vec<i32> = (0..n).map(|i| (i * 7) % 100 - 20).collect();
        for low in [-10i32, 0, 15, 50] {
            let mut ref_mask = 0u32;
            for (j, &v) in mag.iter().enumerate() {
                if v > low {
                    ref_mask |= 1 << j;
                }
            }
            if has_avx2() {
                // SAFETY: 测试环境已探测 AVX2；长度 32 满足契约
                let m = unsafe { crate::simd_kernels::x86::avx2::canny_low_mask32(&mag, low) };
                assert_eq!(m, ref_mask, "avx2 mask low={low}");
            }
            if has_avx512() {
                // SAFETY: 测试环境已探测 AVX512；长度 32 满足契约
                let m = unsafe { crate::simd_kernels::x86::avx512::canny_low_mask32(&mag, low) };
                assert_eq!(m, ref_mask, "avx512 mask low={low}");
            }
        }
    }

    /// 三路径交叉验证：finalPass。
    #[test]
    fn canny_final_pass_cross_isa() {
        for w in [1usize, 5, 16, 17, 32, 33, 64, 65, 100] {
            let mut pmap = vec![1u8; w + 2];
            let mut x = 42u64;
            for v in pmap.iter_mut().take(w + 1).skip(1) {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                *v = match (x >> 33) % 3 {
                    0 => 0,
                    1 => 1,
                    _ => 2,
                };
            }
            let mut ref_dst = vec![0u8; w];
            final_pass_row(&pmap, &mut ref_dst, w);
            if has_avx2() {
                let mut dst = vec![0u8; w];
                // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                unsafe { crate::simd_kernels::x86::avx2::canny_final_pass_row(&pmap, &mut dst, w) };
                assert_eq!(dst, ref_dst, "avx2 final w={w}");
            }
            if has_avx512() {
                let mut dst = vec![0u8; w];
                // SAFETY: 测试环境已探测 AVX512；切片长度与内核契约一致
                unsafe {
                    crate::simd_kernels::x86::avx512::canny_final_pass_row(&pmap, &mut dst, w)
                };
                assert_eq!(dst, ref_dst, "avx512 final w={w}");
            }
        }
    }
}

/// 批内整块写 1（对齐 OpenCV `v_store_aligned(_pmap+j, v_one)`）。
pub fn fill_one_row(pmap: &mut [u8], n: usize) {
    pmap[..n].fill(1);
}

/// NMS 单候选处理（对齐 canny.cpp:556-589，供 nms 内核内联调用）。
///
/// 裸指针版（无 bounds check）：调用方保证 k ∈ [0, w) 且各缓冲长度 ≥ w+2。
///
/// # Safety（调用方契约）
/// - `k < w`，`mag_p`/`mag_a`/`mag_n` 长度 ≥ w+2，`dx`/`dy` 长度 ≥ w，
///   `pmap` 长度 ≥ w+2
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn nms_one(
    mag_p: *const i32,
    mag_a: *const i32,
    mag_n: *const i32,
    dx: *const i16,
    dy: *const i16,
    pmap: *mut u8,
    k: usize,
    high: i32,
    stack: &mut Vec<usize>,
) {
    const TG22: i32 = 13573;
    // SAFETY: 调用方契约保证索引范围
    unsafe {
        let m = *mag_a.add(k + 1);
        let xs = *dx.add(k);
        let ys = *dy.add(k);
        let x = xs.wrapping_abs() as i32;
        let y = (ys.wrapping_abs() as i32) << 15;
        let tg22x = x * TG22;
        let mut is_edge = false;
        if y < tg22x {
            // 水平：左右邻
            if m > *mag_a.add(k) && m >= *mag_a.add(k + 2) {
                is_edge = true;
            }
        } else {
            let tg67x = tg22x + (x << 16);
            if y > tg67x {
                // 垂直：上下邻
                if m > *mag_p.add(k + 1) && m >= *mag_n.add(k + 1) {
                    is_edge = true;
                }
            } else {
                // 对角
                let s: i32 = if (xs ^ ys) < 0 { -1 } else { 1 };
                let sp = (k as i32 + 1 - s) as usize;
                let sn = (k as i32 + 1 + s) as usize;
                if m > *mag_p.add(sp) && m > *mag_n.add(sn) {
                    is_edge = true;
                }
            }
        }
        if is_edge {
            if m > high {
                *pmap.add(k + 1) = 2;
                stack.push(k + 1);
            } else {
                *pmap.add(k + 1) = 0;
            }
        } else {
            *pmap.add(k + 1) = 1;
        }
    }
}

/// NMS + 双阈值整行（标量基准：批预筛 + 候选处理 + 尾部）。
///
/// `mag_*`/`pmap` 长度 w+2（哨兵），有效区 [1, w]；`dx`/`dy` 长度 w。
/// 栈元素为 pmap 相对偏移（列 1..w+1）。
#[allow(clippy::too_many_arguments)]
pub fn nms_full_row(
    mag_p: &[i32],
    mag_a: &[i32],
    mag_n: &[i32],
    dx: &[i16],
    dy: &[i16],
    pmap: &mut [u8],
    w: usize,
    low: i32,
    high: i32,
    stack: &mut Vec<usize>,
) {
    pmap[0] = 1;
    pmap[w + 1] = 1;
    let mut k = 0usize;
    while k < w {
        if mag_a[k + 1] > low {
            // SAFETY: k < w，切片长度 ≥ w+2 由函数契约保证
            unsafe {
                nms_one(
                    mag_p.as_ptr(), mag_a.as_ptr(), mag_n.as_ptr(), dx.as_ptr(), dy.as_ptr(),
                    pmap.as_mut_ptr(), k, high, stack,
                );
            }
        } else {
            pmap[k + 1] = 1;
        }
        k += 1;
    }
}

#[cfg(test)]
mod nms_isa_cross_tests {
    use super::*;

    fn has_avx2() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }
    fn has_avx512() -> bool {
        std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl")
    }

    fn rand_i16(n: usize, seed: u64) -> Vec<i16> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 33) as i32 % 2000 - 1000) as i16
            })
            .collect()
    }

    /// 三路径交叉验证：NMS 整行（含批 + 尾部 + 候选）。
    #[test]
    fn canny_nms_row_cross_isa() {
        for w in [1usize, 5, 31, 32, 33, 63, 64, 65, 100, 128] {
            let mag_w = w + 2;
            let mag_p: Vec<i32> = rand_i16(mag_w, 1).iter().map(|&v| v as i32 * 3).collect();
            let mag_a: Vec<i32> = rand_i16(mag_w, 2).iter().map(|&v| v as i32 * 3).collect();
            let mag_n: Vec<i32> = rand_i16(mag_w, 3).iter().map(|&v| v as i32 * 3).collect();
            let dx = rand_i16(w, 4);
            let dy = rand_i16(w, 5);
            for (low, high) in [(0i32, 100), (200, 500), (1000, 2000)] {
                let mut ref_pmap = vec![1u8; mag_w];
                let mut ref_stack = Vec::new();
                nms_full_row(
                    &mag_p, &mag_a, &mag_n, &dx, &dy, &mut ref_pmap, w, low, high, &mut ref_stack,
                );
                if has_avx2() {
                    let mut pmap = vec![1u8; mag_w];
                    let mut stack = Vec::new();
                    // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                    unsafe {
                        crate::simd_kernels::x86::avx2::canny_nms_row(
                            &mag_p, &mag_a, &mag_n, &dx, &dy, &mut pmap, w, low, high,
                            &mut stack,
                        )
                    };
                    assert_eq!(pmap, ref_pmap, "avx2 nms w={w} low={low}");
                    assert_eq!(stack, ref_stack, "avx2 nms stack w={w} low={low}");
                }
                if has_avx512() {
                    let mut pmap = vec![1u8; mag_w];
                    let mut stack = Vec::new();
                    // SAFETY: 测试环境已探测 AVX512；切片长度与内核契约一致
                    unsafe {
                        crate::simd_kernels::x86::avx512::canny_nms_row(
                            &mag_p, &mag_a, &mag_n, &dx, &dy, &mut pmap, w, low, high,
                            &mut stack,
                        )
                    };
                    assert_eq!(pmap, ref_pmap, "avx512 nms w={w} low={low}");
                    assert_eq!(stack, ref_stack, "avx512 nms stack w={w} low={low}");
                }
            }
        }
    }
}

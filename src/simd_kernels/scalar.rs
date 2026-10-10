//! 标量 fallback（所有平台的正确性基准）。
//!
//! 与 OpenCV5 逐位一致：`gray = (R·9798 + G·19235 + B·3735 + 16384) >> 15`。

/// RGB u8 → 灰度 u8（标量，逐像素）。
///
/// 输入 `src` 为 RGB 交错（每像素 3 字节），输出 `dst` 为灰度（每像素 1 字节）。
/// `n` 为像素数。`src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n`。
pub fn to_grayscale_u8(src: &[u8], dst: &mut [u8], n: usize) {
    for i in 0..n {
        let r = src[i * 3] as u32;
        let g = src[i * 3 + 1] as u32;
        let b = src[i * 3 + 2] as u32;
        dst[i] = ((r * 9798 + g * 19235 + b * 3735 + 16384) >> 15) as u8;
    }
}

/// u8 阈值化（标量，256 项查找表，与 OpenCV thresh.cpp 尾循环一致）。
///
/// `t`/`mv` 已是整数化后的阈值/最大值。`n` 为像素数（元素数）。
pub fn threshold_u8(src: &[u8], dst: &mut [u8], n: usize, t: u8, mv: u8, type_: u8) {
    // type_: 0=Binary 1=BinaryInv 2=Trunc 3=ToZero 4=ToZeroInv
    let pivot = t as usize + 1;
    let mut tab = [0u8; 256];
    match type_ {
        0 => {
            // src > t ? mv : 0
            tab[..pivot.min(256)].fill(0);
            if pivot < 256 {
                tab[pivot..].fill(mv);
            }
        }
        1 => {
            // src <= t ? mv : 0
            tab[..pivot.min(256)].fill(mv);
            if pivot < 256 {
                tab[pivot..].fill(0);
            }
        }
        2 => {
            // min(src, t)
            for (i, v) in tab.iter_mut().enumerate().take(pivot.min(256)) {
                *v = i as u8;
            }
            if pivot < 256 {
                tab[pivot..].fill(t);
            }
        }
        3 => {
            // src > t ? src : 0
            tab[..pivot.min(256)].fill(0);
            if pivot < 256 {
                for (i, v) in tab.iter_mut().enumerate().take(256).skip(pivot) {
                    *v = i as u8;
                }
            }
        }
        4 => {
            // src <= t ? src : 0
            for (i, v) in tab.iter_mut().enumerate().take(pivot.min(256)) {
                *v = i as u8;
            }
            if pivot < 256 {
                tab[pivot..].fill(0);
            }
        }
        _ => unreachable!(),
    }
    for i in 0..n {
        dst[i] = tab[src[i] as usize];
    }
}

/// 自适应阈值化（标量，对应 OpenCV `getThreshVal_Adapt` 尾循环）。
///
/// `src - mean > -idelta ? imax : 0`（Binary）/ `src - mean <= -idelta ? imax : 0`（BinaryInv）。
/// 差值按 i32 计算（u8 - u8 ∈ [-255, 255]，无溢出）。
pub fn adaptive_thresh_u8(
    src: &[u8],
    mean: &[u8],
    dst: &mut [u8],
    n: usize,
    imax: u8,
    idelta: i32,
    inv: bool,
) {
    let thr = -idelta;
    if inv {
        for i in 0..n {
            let d = src[i] as i32 - mean[i] as i32;
            dst[i] = if d <= thr { imax } else { 0 };
        }
    } else {
        for i in 0..n {
            let d = src[i] as i32 - mean[i] as i32;
            dst[i] = if d > thr { imax } else { 0 };
        }
    }
}

// ---- 按位逻辑（bitwise.rs 的标量基准）----

pub fn bitwise_and_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    for i in 0..n {
        dst[i] = a[i] & b[i];
    }
}

pub fn bitwise_or_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    for i in 0..n {
        dst[i] = a[i] | b[i];
    }
}

pub fn bitwise_xor_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    for i in 0..n {
        dst[i] = a[i] ^ b[i];
    }
}

pub fn bitwise_not_bytes(a: &[u8], dst: &mut [u8], n: usize) {
    for i in 0..n {
        dst[i] = !a[i];
    }
}

// ---- 连通域（connected_components.rs 的标量基准）----

/// 第二遍重写内核：`labels[i] = p[labels[i]]`（原地）。
///
/// # Safety（调用方契约）
/// `labels` 的每个元素必须是 `p` 的合法下标（`0 <= v < p.len()`）。
pub fn cc_relabel_inplace(labels: &mut [i32], p: &[i32]) {
    let n = labels.len();
    let lp = labels.as_mut_ptr();
    let pp = p.as_ptr();
    let mut i: usize = 0;
    // SAFETY: 调用方契约保证 labels 值域在 p 内；i < n 时 lp.add/pp.offset 合法
    unsafe {
        while i + 8 <= n {
            for k in 0..8 {
                let v = *lp.add(i + k);
                *lp.add(i + k) = *pp.offset(v as isize);
            }
            i += 8;
        }
        while i < n {
            let v = *lp.add(i);
            *lp.add(i) = *pp.offset(v as isize);
            i += 1;
        }
    }
}

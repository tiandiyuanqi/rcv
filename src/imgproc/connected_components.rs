//! 连通域标记（对应 OpenCV `cv::connectedComponents/WithStats`）。
//!
//! 对齐 connectedcomponents.cpp 的 **SAUF（Wu）算法**（CCL_WU/CCL_SAUF）：
//! 两遍扫描 + union-find 等价类合并。OpenCV 5.0 默认 CCL_DEFAULT 走
//! Spaghetti（DAG 决策树），但 SAUF 是其基础且标签编号语义一致
//! （背景=0，前景标签按扫描序 1..n 连续）。
//!
//! 注意：不同 CCL 算法的**标签编号顺序**可能不同（同一连通域在不同
//! 算法下可能拿到不同编号），但划分（哪些像素同域）一致。对拍时
//! 以划分为准（或固定用 SAUF 对 SAUF）。

use crate::core::image::Image;

/// 连通域统计（对应 `CCStatsOp` 的 stats 行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CCStat {
    /// 包围盒左上角 x（`CC_STAT_LEFT`）。
    pub left: i32,
    /// 包围盒左上角 y（`CC_STAT_TOP`）。
    pub top: i32,
    /// 包围盒宽（`CC_STAT_WIDTH`）。
    pub width: i32,
    /// 包围盒高（`CC_STAT_HEIGHT`）。
    pub height: i32,
    /// 像素数（`CC_STAT_AREA`）。
    pub area: i32,
}

/// 连通域标记结果。
pub struct ConnectedComponents {
    /// 标签图（背景=0，前景 1..=nlabels-1）。
    pub labels: Image<i32, 1>,
    /// 标签数（含背景；有效前景标签 nlabels-1 个）。
    pub nlabels: usize,
    /// 各标签统计（`with_stats=true` 时有效，长度 nlabels）。
    pub stats: Vec<CCStat>,
    /// 各标签质心（`with_stats=true` 时有效；背景为 NaN，对齐 OpenCV）。
    pub centroids: Vec<(f64, f64)>,
}

/// 连通方式（对应 `connectivity` 参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    /// 4 连通（上下左右）。
    Connected4,
    /// 8 连通（含对角）。
    Connected8,
}

#[inline]
fn find_root(p: &[i32], mut i: i32) -> i32 {
    while p[i as usize] < i {
        i = p[i as usize];
    }
    i
}

#[inline]
fn set_root(p: &mut [i32], mut i: i32, root: i32) {
    while p[i as usize] < i {
        let j = p[i as usize];
        p[i as usize] = root;
        i = j;
    }
    p[i as usize] = root;
}

#[inline]
fn set_union(p: &mut [i32], i: i32, j: i32) -> i32 {
    let mut root = find_root(p, i);
    if i != j {
        let rootj = find_root(p, j);
        if root > rootj {
            root = rootj;
        }
        set_root(p, j, root);
    }
    set_root(p, i, root);
    root
}

/// 带偏移的 set_union（条带局部 P 切片，标签 l → p_local[l - first]）。
#[inline]
fn set_union_local(p_local: &mut [i32], first: i32, i: i32, j: i32) -> i32 {
    let find_root = |p: &[i32], i: i32| -> i32 {
        let mut root = i;
        while p[(root - first) as usize] < root {
            root = p[(root - first) as usize];
        }
        root
    };
    let set_root = |p: &mut [i32], mut i: i32, root: i32| {
        while p[(i - first) as usize] < i {
            let j = p[(i - first) as usize];
            p[(i - first) as usize] = root;
            i = j;
        }
        p[(i - first) as usize] = root;
    };
    let mut root = find_root(p_local, i);
    if i != j {
        let rootj = find_root(p_local, j);
        if root > rootj {
            root = rootj;
        }
        set_root(p_local, j, root);
    }
    set_root(p_local, i, root);
    root
}

/// 展平 union-find 树并重编号（对齐 flattenL）。
fn flatten_l(p: &mut [i32], length: usize) -> usize {
    let mut k = 1usize;
    for i in 1..length {
        if p[i] < i as i32 {
            p[i] = p[p[i] as usize];
        } else {
            p[i] = k as i32;
            k += 1;
        }
    }
    k
}

/// 连通域标记（对应 `cv::connectedComponents`，SAUF 算法）。
///
/// 输入为 8 位单通道图（非零 = 前景）。返回标签图与标签数。
pub fn connected_components(src: &Image<u8, 1>, connectivity: Connectivity) -> ConnectedComponents {
    connected_components_impl(src, connectivity, false)
}

/// 连通域标记 + 统计（对应 `cv::connectedComponentsWithStats`）。
///
/// 额外返回每个标签的包围盒/面积与质心（背景标签 0 的质心为 NaN，
/// 对齐 OpenCV）。
pub fn connected_components_with_stats(
    src: &Image<u8, 1>,
    connectivity: Connectivity,
) -> ConnectedComponents {
    connected_components_impl(src, connectivity, true)
}

#[allow(clippy::manual_div_ceil)] // (w*h+1)/2+1 保留 OpenCV 原式（Plength）
fn connected_components_impl(
    src: &Image<u8, 1>,
    connectivity: Connectivity,
    with_stats: bool,
) -> ConnectedComponents {
    let (w, h) = (src.width(), src.height());
    // labels 未初始化分配：pass1 对每个像素都写入（bg=0/fg=标签），全覆盖。
    let mut labels: Image<i32, 1> = Image::new_uninit(w, h);
    // 标签上界：(w*h+1)/2 + 1（对齐 LabelingWu）。
    // p 无需清零：pass1 分配的标签 [1, lunique) 连续且全部写自指，
    // flatten 只遍历 [1, lunique)；p[0] 单独清零（背景）。
    let plength = (w * h + 1) / 2 + 1;
    let mut p = crate::simd_kernels::alloc_uninit(plength);
    p[0] = 0;
    let mut lunique: i32 = 1;

    let sdata = src.data();
    let ldata = labels.data_mut();

    // MT 路径（对齐 LabelingWuParallel）：行数 ≥ 2×线程数时条带并行。
    // 每条带独立标签空间（stripeFirstLabel = y/2*(w+1)/2+1），边界行 merge。
    let nthreads = rayon::current_num_threads();
    if h >= nthreads.saturating_mul(2).max(2) && nthreads > 1 {
        return cc_parallel(src, connectivity, with_stats, nthreads);
    }

    match connectivity {
        Connectivity::Connected8 => {
            for r in 0..h {
                let img_row = &sdata[r * w..(r + 1) * w];
                let img_row_prev: &[u8] = if r > 0 {
                    &sdata[(r - 1) * w..r * w]
                } else {
                    &[]
                };
                let (labels_row_prev, labels_row) = if r > 0 {
                    let (prev, cur) = ldata.split_at_mut(r * w);
                    (&prev[(r - 1) * w..], &mut cur[..w])
                } else {
                    (&[][..], &mut ldata[..w])
                };
                for c in 0..w {
                    // condition_x
                    if img_row[c] > 0 {
                        // condition_q: r>0 && prev[c]>0
                        let q = r > 0 && img_row_prev[c] > 0;
                        if q {
                            labels_row[c] = labels_row_prev[c];
                        } else {
                            // condition_r: c<w-1 && r>0 && prev[c+1]>0
                            let rr = c < w - 1 && r > 0 && img_row_prev[c + 1] > 0;
                            if rr {
                                // condition_p: c>0 && r>0 && prev[c-1]>0
                                let pp = c > 0 && r > 0 && img_row_prev[c - 1] > 0;
                                if pp {
                                    labels_row[c] =
                                        set_union(&mut p, labels_row_prev[c - 1], labels_row_prev[c + 1]);
                                } else {
                                    // condition_s: c>0 && cur[c-1]>0
                                    let s = c > 0 && img_row[c - 1] > 0;
                                    if s {
                                        labels_row[c] =
                                            set_union(&mut p, labels_row[c - 1], labels_row_prev[c + 1]);
                                    } else {
                                        labels_row[c] = labels_row_prev[c + 1];
                                    }
                                }
                            } else {
                                // condition_p
                                let pp = c > 0 && r > 0 && img_row_prev[c - 1] > 0;
                                if pp {
                                    labels_row[c] = labels_row_prev[c - 1];
                                } else {
                                    let s = c > 0 && img_row[c - 1] > 0;
                                    if s {
                                        labels_row[c] = labels_row[c - 1];
                                    } else {
                                        labels_row[c] = lunique;
                                        p[lunique as usize] = lunique;
                                        lunique += 1;
                                    }
                                }
                            }
                        }
                    } else {
                        labels_row[c] = 0;
                    }
                }
            }
        }
        Connectivity::Connected4 => {
            for r in 0..h {
                let img_row = &sdata[r * w..(r + 1) * w];
                let img_row_prev: &[u8] = if r > 0 {
                    &sdata[(r - 1) * w..r * w]
                } else {
                    &[]
                };
                let (labels_row_prev, labels_row) = if r > 0 {
                    let (prev, cur) = ldata.split_at_mut(r * w);
                    (&prev[(r - 1) * w..], &mut cur[..w])
                } else {
                    (&[][..], &mut ldata[..w])
                };
                for c in 0..w {
                    if img_row[c] > 0 {
                        let q = r > 0 && img_row_prev[c] > 0;
                        let s = c > 0 && img_row[c - 1] > 0;
                        if q {
                            if s {
                                labels_row[c] = set_union(&mut p, labels_row[c - 1], labels_row_prev[c]);
                            } else {
                                labels_row[c] = labels_row_prev[c];
                            }
                        } else if s {
                            labels_row[c] = labels_row[c - 1];
                        } else {
                            labels_row[c] = lunique;
                            p[lunique as usize] = lunique;
                            lunique += 1;
                        }
                    } else {
                        labels_row[c] = 0;
                    }
                }
            }
        }
    }

    // 展平 + 第二遍重写
    let nlabels = flatten_l(&mut p, lunique as usize);

    // stats 累积（第二遍同步进行，对齐 CCStatsOp）
    let mut stats = Vec::new();
    let mut integrals = Vec::new(); // (sum_x, sum_y) u64
    let mut stats_final: Vec<(f64, f64)> = Vec::new();
    if with_stats {
        stats.resize(
            nlabels,
            CCStat {
                left: i32::MAX,
                top: i32::MAX,
                width: i32::MIN,
                height: i32::MIN,
                area: 0,
            },
        );
        integrals.resize(nlabels, (0u64, 0u64));
    }

    if with_stats {
        for r in 0..h {
            for c in 0..w {
                let idx = r * w + c;
                let l = p[ldata[idx] as usize];
                ldata[idx] = l;
                let st = &mut stats[l as usize];
                st.left = st.left.min(c as i32);
                st.width = st.width.max(c as i32);
                st.top = st.top.min(r as i32);
                st.height = st.height.max(r as i32);
                st.area += 1;
                integrals[l as usize].0 += c as u64;
                integrals[l as usize].1 += r as u64;
            }
        }
    } else {
        // SAFETY: pass1 写入的标签值 ∈ [0, lunique) ⊆ [0, p.len())
        crate::simd_kernels::cc_relabel_inplace(ldata, &p);
    }

    if with_stats {
        // finish：width/height 转尺寸，质心 = integral/area
        let mut centroids = vec![(f64::NAN, f64::NAN); nlabels];
        for l in 0..nlabels {
            let st = &mut stats[l];
            if st.area > 0 {
                st.width = st.width - st.left + 1;
                st.height = st.height - st.top + 1;
                let area = st.area as f64;
                centroids[l] = (integrals[l].0 as f64 / area, integrals[l].1 as f64 / area);
            } else {
                st.width = 0;
                st.height = 0;
                st.left = -1;
            }
        }
        stats_final = centroids;
    }
    let centroids = stats_final;

    ConnectedComponents {
        labels,
        nlabels,
        stats,
        centroids,
    }
}


/// 条带并行 SAUF（对齐 LabelingWuParallel）。
///
/// 1. 条带划分（偶数行对齐，每条带 ≥2 行）
/// 2. 第一遍：条带并行扫描，每条带独立标签空间
///    （stripeFirstLabel = r0/2·((w+1)/2)+1，条带内 limit_line=r0 不看上行）
/// 3. 边界 merge：每条带首行与上一条带末行做 p/q/r 合并
/// 4. flattenL + 并行第二遍（stats 版按条带局部累积再合并）
#[allow(clippy::too_many_lines)]
#[allow(clippy::manual_div_ceil)] // (w*h+1)/2+1 保留 OpenCV 原式（Plength）
fn cc_parallel(
    src: &Image<u8, 1>,
    connectivity: Connectivity,
    with_stats: bool,
    nthreads: usize,
) -> ConnectedComponents {
    let (w, h) = (src.width(), src.height());
    let mut labels: Image<i32, 1> = Image::new(w, h);
    let plength = (w * h + 1) / 2 + 1;
    let mut p = vec![0i32; plength];

    // 条带划分：nstripes 条，边界对齐偶数行（stripeFirstLabel 要求）。
    // 条带数上限 16：实测更多条带时 merge/flatten 串行段与缓存争用反噬
    // （纯扫描带宽峰值也在 8-16 条带）。
    let nstripes = nthreads.min(h / 2).clamp(1, 16);
    let base_rows = h / nstripes;
    let mut stripe_infos: Vec<(usize, usize)> = Vec::with_capacity(nstripes);
    {
        let mut r0 = 0usize;
        for i in 0..nstripes {
            let mut r1 = if i + 1 == nstripes {
                h
            } else {
                let t = (i + 1) * base_rows;
                // 偶数对齐
                if t % 2 == 1 {
                    t + 1
                } else {
                    t
                }
                .min(h)
            };
            if i + 1 == nstripes {
                r1 = h;
            }
            if r1 > r0 {
                stripe_infos.push((r0, r1));
                r0 = r1;
            }
        }
    }

    let sdata = src.data();
    let ldata = labels.data_mut();

    // ---- 第一遍：条带并行 ----
    // 把 ldata 按条带切成不相交 &mut [i32]（split_at_mut 链）
    let mut stripe_slices: Vec<&mut [i32]> = Vec::with_capacity(stripe_infos.len());
    {
        let mut rest: &mut [i32] = ldata;
        for &(r0, r1) in &stripe_infos {
            let (head, tail) = rest.split_at_mut((r1 - r0) * w);
            stripe_slices.push(head);
            rest = tail;
        }
    }

    use rayon::prelude::*;
    // p 按条带标签空间切分（stripeFirstLabel 保证不相交）：
    // 条带 si 的空间 [first(si), first(si+1))，末条带到 plength。
    let stripe_first: Vec<i32> = stripe_infos
        .iter()
        .map(|&(r0, _)| (r0 as i32 / 2) * ((w as i32 + 1) / 2) + 1)
        .collect();
    // p_slices[i] = p[stripe_first[i] .. next_first or plength)，
    // p_local[(l - first) as usize] 即 p[l]。
    // 先跳过前缀 p[0..stripe_first[0])（stripe_first[0] == 1，即背景槽 p[0]）。
    let mut p_slices: Vec<&mut [i32]> = Vec::with_capacity(stripe_infos.len());
    {
        let mut rest: &mut [i32] = &mut p[stripe_first[0] as usize..];
        for i in 0..stripe_infos.len() {
            let end = if i + 1 < stripe_infos.len() {
                stripe_first[i + 1] as usize
            } else {
                plength
            };
            let len = end - stripe_first[i] as usize;
            let (head, tail) = rest.split_at_mut(len);
            p_slices.push(head);
            rest = tail;
        }
    }

    // 每条带实际用到的标签数（对齐 chunksSizeAndLabels[startR+1]）
    let stripe_used = vec![0i32; stripe_infos.len()];
    let stripe_used = std::sync::Mutex::new(stripe_used);
    let stripe_pairs: Vec<(&mut [i32], &mut [i32])> =
        stripe_slices.into_iter().zip(p_slices).collect();
    stripe_pairs
        .into_par_iter()
        .enumerate()
        .for_each(|(si, (stripe_l, p_local))| {
            let (r0, r1) = stripe_infos[si];
            let p_first = stripe_first[si];
            // 局部包装：标签 l → p_local[(l - p_first) as usize]
            let mut lunique: i32 = p_first;
            let limit_line = r0;
            for r in r0..r1 {
                let img_row = &sdata[r * w..(r + 1) * w];
                let img_row_prev: &[u8] = if r > limit_line {
                    &sdata[(r - 1) * w..r * w]
                } else {
                    &[]
                };
                let off = (r - r0) * w;
                let (labels_row_prev, labels_row) = if r > limit_line {
                    let (prev, cur) = stripe_l.split_at_mut(off);
                    (&prev[off - w..], &mut cur[..w])
                } else {
                    (&[][..], &mut stripe_l[off..off + w])
                };
                match connectivity {
                    Connectivity::Connected8 => {
                        for c in 0..w {
                            if img_row[c] > 0 {
                                let q = r > limit_line && img_row_prev[c] > 0;
                                if q {
                                    labels_row[c] = labels_row_prev[c];
                                } else {
                                    let rr = c < w - 1 && r > limit_line && img_row_prev[c + 1] > 0;
                                    if rr {
                                        let pp = c > 0 && r > limit_line && img_row_prev[c - 1] > 0;
                                        if pp {
                                            labels_row[c] = set_union_local(
                                                p_local,
                                                p_first,
                                                labels_row_prev[c - 1],
                                                labels_row_prev[c + 1],
                                            );
                                        } else {
                                            let s = c > 0 && img_row[c - 1] > 0;
                                            if s {
                                                labels_row[c] = set_union_local(
                                                    p_local,
                                                    p_first,
                                                    labels_row[c - 1],
                                                    labels_row_prev[c + 1],
                                                );
                                            } else {
                                                labels_row[c] = labels_row_prev[c + 1];
                                            }
                                        }
                                    } else {
                                        let pp = c > 0 && r > limit_line && img_row_prev[c - 1] > 0;
                                        if pp {
                                            labels_row[c] = labels_row_prev[c - 1];
                                        } else {
                                            let s = c > 0 && img_row[c - 1] > 0;
                                            if s {
                                                labels_row[c] = labels_row[c - 1];
                                            } else {
                                                labels_row[c] = lunique;
                                                p_local[(lunique - p_first) as usize] = lunique;
                                                lunique += 1;
                                            }
                                        }
                                    }
                                }
                            } else {
                                labels_row[c] = 0;
                            }
                        }
                    }
                    Connectivity::Connected4 => {
                        for c in 0..w {
                            if img_row[c] > 0 {
                                let q = r > limit_line && img_row_prev[c] > 0;
                                let s = c > 0 && img_row[c - 1] > 0;
                                if q {
                                    if s {
                                        labels_row[c] = set_union_local(
                                            p_local,
                                            p_first,
                                            labels_row[c - 1],
                                            labels_row_prev[c],
                                        );
                                    } else {
                                        labels_row[c] = labels_row_prev[c];
                                    }
                                } else if s {
                                    labels_row[c] = labels_row[c - 1];
                                } else {
                                    labels_row[c] = lunique;
                                    p_local[(lunique - p_first) as usize] = lunique;
                                    lunique += 1;
                                }
                            } else {
                                labels_row[c] = 0;
                            }
                        }
                    }
                }
            }
            stripe_used.lock().unwrap()[si] = lunique - p_first;
        });

    let stripe_used = stripe_used.into_inner().unwrap();

    // ---- 边界 merge（对齐 mergeLabels8/4Connectivity）----
    for (i, &(r0, _)) in stripe_infos.iter().enumerate() {
        if i == 0 {
            continue;
        }
        let r = r0;
        let (prev_part, cur_part) = ldata.split_at_mut(r * w);
        let labels_row_prev = &prev_part[(r - 1) * w..];
        let labels_row = &mut cur_part[..w];
        match connectivity {
            Connectivity::Connected8 => {
                for c in 0..w {
                    if labels_row[c] > 0 {
                        if c > 0 && labels_row_prev[c - 1] > 0 {
                            labels_row[c] = set_union(&mut p, labels_row_prev[c - 1], labels_row[c]);
                        }
                        if c < w - 1 && labels_row_prev[c + 1] > 0 {
                            labels_row[c] = set_union(&mut p, labels_row_prev[c + 1], labels_row[c]);
                        }
                        if labels_row_prev[c] > 0 {
                            labels_row[c] = set_union(&mut p, labels_row_prev[c], labels_row[c]);
                        }
                    }
                }
            }
            Connectivity::Connected4 => {
                for c in 0..w {
                    if labels_row[c] > 0 && labels_row_prev[c] > 0 {
                        labels_row[c] = set_union(&mut p, labels_row_prev[c], labels_row[c]);
                    }
                }
            }
        }
    }

    // ---- flatten（对齐 flattenLParallel：只遍历各条带实际用到的
    // [first, first+used)，未用槽不碰）----
    let mut k: i32 = 1;
    for i in 0..stripe_infos.len() {
        let start = stripe_first[i] as usize;
        let end = start + stripe_used[i] as usize;
        for j in start..end {
            if p[j] < j as i32 {
                p[j] = p[p[j] as usize];
            } else {
                p[j] = k;
                k += 1;
            }
        }
    }
    let nlabels = k as usize;

    // ---- 第二遍（+ stats）----
    if with_stats {
        // 第二遍也按条带切 ldata
        let mut stats_stripe_slices: Vec<&mut [i32]> = Vec::with_capacity(stripe_infos.len());
        {
            let mut rest: &mut [i32] = ldata;
            for &(r0, r1) in &stripe_infos {
                let (head, tail) = rest.split_at_mut((r1 - r0) * w);
                stats_stripe_slices.push(head);
                rest = tail;
            }
        }
        /// 条带局部统计：(stats, (sum_x, sum_y))
        type StripeStats = (Vec<CCStat>, Vec<(u64, u64)>);
        let results: Vec<StripeStats> = stats_stripe_slices
            .into_par_iter()
            .enumerate()
            .map(|(si, stripe_l)| {
                let (r0, r1) = stripe_infos[si];
                let mut st = vec![
                    CCStat {
                        left: i32::MAX,
                        top: i32::MAX,
                        width: i32::MIN,
                        height: i32::MIN,
                        area: 0,
                    };
                    nlabels
                ];
                let mut itg = vec![(0u64, 0u64); nlabels];
                for r in r0..r1 {
                    for c in 0..w {
                        let idx = (r - r0) * w + c;
                        let l = p[stripe_l[idx] as usize];
                        stripe_l[idx] = l;
                        let s = &mut st[l as usize];
                        s.left = s.left.min(c as i32);
                        s.width = s.width.max(c as i32);
                        s.top = s.top.min(r as i32);
                        s.height = s.height.max(r as i32);
                        s.area += 1;
                        itg[l as usize].0 += c as u64;
                        itg[l as usize].1 += r as u64;
                    }
                }
                (st, itg)
            })
            .collect();
        let stripe_stats: Vec<Vec<CCStat>> = results.iter().map(|(s, _)| s.clone()).collect();
        let stripe_integrals: Vec<Vec<(u64, u64)>> =
            results.into_iter().map(|(_, i)| i).collect();
        let mut stats = vec![
            CCStat {
                left: i32::MAX,
                top: i32::MAX,
                width: i32::MIN,
                height: i32::MIN,
                area: 0,
            };
            nlabels
        ];
        let mut integrals = vec![(0u64, 0u64); nlabels];
        for si in 0..stripe_infos.len() {
            for l in 0..nlabels {
                let s = &stripe_stats[si][l];
                if s.area > 0 {
                    let d = &mut stats[l];
                    d.left = d.left.min(s.left);
                    d.width = d.width.max(s.width);
                    d.top = d.top.min(s.top);
                    d.height = d.height.max(s.height);
                    d.area += s.area;
                    integrals[l].0 += stripe_integrals[si][l].0;
                    integrals[l].1 += stripe_integrals[si][l].1;
                }
            }
        }
        let mut centroids = vec![(f64::NAN, f64::NAN); nlabels];
        for l in 0..nlabels {
            let st = &mut stats[l];
            if st.area > 0 {
                st.width = st.width - st.left + 1;
                st.height = st.height - st.top + 1;
                let area = st.area as f64;
                centroids[l] = (integrals[l].0 as f64 / area, integrals[l].1 as f64 / area);
            } else {
                st.width = 0;
                st.height = 0;
                st.left = -1;
            }
        }
        return ConnectedComponents {
            labels,
            nlabels,
            stats,
            centroids,
        };
    }

    // 无 stats：并行重写（每条带一个内核调用，消除边界检查）
    let chunk_rows = (h / nstripes).max(1);
    ldata
        .par_chunks_mut(w * chunk_rows)
        .for_each(|chunk| {
            // SAFETY: pass1 写入的标签值 ∈ [0, lunique) ⊆ [0, p.len())
            crate::simd_kernels::cc_relabel_inplace(chunk, &p);
        });

    ConnectedComponents {
        labels,
        nlabels,
        stats: Vec::new(),
        centroids: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: usize, h: usize, data: &[u8]) -> Image<u8, 1> {
        Image::from_vec(w, h, data.to_vec()).unwrap()
    }

    #[test]
    fn two_blobs_8way() {
        // 两个分离的方块
        let im = img(
            6,
            3,
            &[
                1, 1, 0, 0, 1, 0, //
                1, 1, 0, 0, 1, 0, //
                0, 0, 0, 0, 0, 0,
            ],
        );
        let cc = connected_components(&im, Connectivity::Connected8);
        assert_eq!(cc.nlabels, 3); // 背景 + 2 域
        let l = cc.labels.data();
        assert_eq!(l[0], l[1]);
        assert_eq!(l[6], l[7]);
        assert_eq!(l[0], l[6]);
        assert_ne!(l[0], l[4]);
        assert_eq!(l[2], 0);
    }

    #[test]
    fn diagonal_4vs8() {
        // 对角相邻：8 连通为一域，4 连通为两域
        let im = img(
            2,
            2,
            &[1, 0, //
              0, 1],
        );
        let cc8 = connected_components(&im, Connectivity::Connected8);
        assert_eq!(cc8.nlabels, 2);
        assert_eq!(cc8.labels.data()[0], cc8.labels.data()[3]);

        let cc4 = connected_components(&im, Connectivity::Connected4);
        assert_eq!(cc4.nlabels, 3);
        assert_ne!(cc4.labels.data()[0], cc4.labels.data()[3]);
    }

    #[test]
    fn u_shape_merge() {
        // U 形：两臂在底行合并（8 连通）
        let im = img(
            3,
            3,
            &[
                1, 0, 1, //
                1, 0, 1, //
                1, 1, 1,
            ],
        );
        let cc = connected_components(&im, Connectivity::Connected8);
        assert_eq!(cc.nlabels, 2);
        let l = cc.labels.data();
        for i in [0, 3, 6, 7, 8, 5, 2] {
            assert_eq!(l[i], l[0], "idx {i}");
        }
    }

    #[test]
    fn stats_and_centroids() {
        let im = img(
            4,
            3,
            &[
                0, 1, 1, 0, //
                0, 1, 1, 0, //
                0, 0, 0, 0,
            ],
        );
        let cc = connected_components_with_stats(&im, Connectivity::Connected8);
        assert_eq!(cc.nlabels, 2);
        let st = cc.stats[1];
        assert_eq!(st.left, 1);
        assert_eq!(st.top, 0);
        assert_eq!(st.width, 2);
        assert_eq!(st.height, 2);
        assert_eq!(st.area, 4);
        let (cx, cy) = cc.centroids[1];
        assert!((cx - 1.5).abs() < 1e-12 && (cy - 0.5).abs() < 1e-12);
        // 背景（标签 0）也有 stats：背景像素的包围盒/面积/质心（对齐 OpenCV）
        assert_eq!(cc.stats[0].area, 8);
        assert_eq!(cc.stats[0].left, 0);
        assert!((cc.centroids[0].0 - 1.5).abs() < 1e-12);
    }

    #[test]
    fn empty_and_full() {
        let im = img(3, 2, &[0; 6]);
        let cc = connected_components(&im, Connectivity::Connected8);
        assert_eq!(cc.nlabels, 1);
        assert!(cc.labels.data().iter().all(|&v| v == 0));

        let im = img(3, 2, &[255; 6]);
        let cc = connected_components(&im, Connectivity::Connected4);
        assert_eq!(cc.nlabels, 2);
        assert!(cc.labels.data().iter().all(|&v| v == 1));
    }

    #[test]
    fn labels_contiguous() {
        // 标签必须 1..=n-1 连续（flattenL 保证）
        let im = img(
            5,
            2,
            &[
                1, 0, 1, 0, 1, //
                0, 1, 0, 1, 0,
            ],
        );
        let cc = connected_components(&im, Connectivity::Connected4);
        let mut seen = vec![false; cc.nlabels];
        for &l in cc.labels.data() {
            seen[l as usize] = true;
        }
        for (i, s) in seen.iter().enumerate() {
            assert!(s, "label {i} missing");
        }
    }
}

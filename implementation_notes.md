# 实现笔记（踩坑记录与工程细节）

> 本文件是 DESIGN.md 的工程补充：记录实现过程中踩过的坑、根因分析、修复方案与实测数据。
> **每完成一个函数/修一个 bug，把可复用的经验追加到对应章节**，供后续阶段（形态学、梯度、Canny…）持续参考。
>
> 约定：所有数据来自 Xeon 64 核（AVX512）实测，`isa()` 返回 Avx512；bench 图 1024×1024 RGB，best-of-5。

---

## 目录

1. [多线程与线程池](#1-多线程与线程池)
2. [SIMD 内核工程规则](#2-simd-内核工程规则)
3. [算法对齐 OpenCV 的语义细节](#3-算法对齐-opencv-的语义细节)
4. [对拍与 bench 方法论](#4-对拍与-bench-方法论)
5. [API 风格决策](#5-api-风格决策)
6. [当前性能基线](#6-当前性能基线)
7. [遗留问题清单](#7-遗留问题清单)

---

## 1. 多线程与线程池

### 1.1 spin_pool 永久自旋 bug（最深的坑）

**现象**：MT bench 病态慢甚至挂起（550s 超时）；to_grayscale MT 只有 26-31 MPix/s（ST 3979）；曾误判"16 线程最快"。

**根因**：`spin_pool.rs` 的 worker 循环 `while EPOCH == local { spin_loop() }` **无停车机制**。首次 MT 调用 spawn ~49 个 worker 后，它们在调用结束后仍以 100% CPU 空转，64 核被自旋线程占满，同进程内 rayon 线程全部饿死。"16 线程最快"是假象——超过 16 线程时 spin worker 开始挤占其他核，从未真正跑起来过。

**修复**（三层，缺一不可）：

1. **停车机制**：worker 自旋超过预算（`SPIN_BUDGET = 2_000_000` 次 ≈ 1ms）后 `thread::park()`；下次 `run_blocks` 持有 HANDLES 锁逐个 `unpark()`。park 前双重检查 EPOCH 防丢失唤醒（unpark 在 epoch 递增后发生，worker park 前最后一次 Acquire 读）。
2. **nblocks 上限 = 并行度**：`nblocks = div_ceil(...).min(rayon::current_num_threads())`。过订阅（64 块挤 64 核 + rayon 64 线程）反而损失带宽。
3. **块大小 16 行 → 64 行**：带宽型内核 16 行块在 MT 下损失 ~2× 带宽（L3 局部性差）；64 行 = 64KB 块。threshold 早就实测过这个结论，但 to_grayscale 没同步——**同类内核的调参结论要横向同步**。

**SPIN_BUDGET 取值**：太小（如 20000 ≈ 几十 µs）会导致 bench 循环间隔内 worker 频繁 park/unpark（唤醒风暴 ~100-400µs），吞吐反而劣化。1ms 量级覆盖典型调用间隔。

**验证方法**：
- 空闲 worker 状态必须是 `S`（sleeping）：`for t in /proc/$PID/task/*; do cat $t/comm; done` 找 rcv-spin-*，`awk '{print $3}' $t/stat` 查状态
- 进程退出后 `ps aux | grep bench` 无残留
- MT bench 能在秒级完成（修复前 550s 超时）

**效果**：to_grayscale MT 2430 → 52051 MPix/s（21×），MT/ST 扩展比 0.6× → 12.8×。

### 1.2 spin_pool 的并发安全

全局静态（EPOCH/SRC/DST/FN/CTX/...）非线程安全，并发调用 `run_blocks` 会互相覆盖。修复：`CALL_LOCK: Mutex<()>` 串行化。测试 `run_blocks_concurrent` 覆盖（4 线程并发不同 ctx，断言各自结果正确）。

**测试陷阱**：并发测试若用 `Arc<Mutex<Vec>>` 收集结果再 `enumerate()` 对位，push 顺序取决于线程完成顺序，会假失败。正确做法：`join()` 返回值直接携带结果。

### 1.3 rayon vs spin_pool 的选型

| 场景 | 选择 | 理由 |
|------|------|------|
| 带宽型小任务（threshold/to_grayscale，1MB 级） | spin_pool | rayon work-stealing fork-join ~33µs/次，超过计算本身（~25µs） |
| 计算密集型（bilateral/median/gaussian） | rayon 行块并行 | 单块计算 ms 级，调度开销占比小 |
| 行阶段 + 列阶段两段式（box_filter） | rayon 两阶段 | mid 缓冲物化后两阶段各自并行 |

### 1.4 box_filter MT 行阶段的两个坑

1. **每行任务内 malloc**：原实现每行任务闭包里 `alloc_uninit(pw * C)`（每行一次分配），1030 个行任务 = 1030 次 malloc。改为 16 行块任务、块级一次分配。
2. **`par_chunks_exact_mut` 丢尾块**：mid 长度 `row_w * (h + kh - 1)`（如 1024×1026）不被 `row_w * 16` 整除时，**尾部 2 行被静默丢弃（未初始化）**，底部输出行错误。对拍 PSNR 39dB 才暴露。改用 `par_chunks_mut` + 内层 `chunks_exact_mut(row_w)`（尾块长度必为 row_w 倍数）。

**教训**：`par_chunks_exact_mut` 要求总长整除，否则尾块静默丢失——凡是缓冲长度 = `h + kh - 1` 这类带边框的，一律用 `par_chunks_mut`。

### 1.5 adaptive_threshold 的 MT 倒退

**现象**：ST 2246 MPix/s，MT 反而 989（0.44×）。

**根因**：结构 = box_filter(灰度) + 末尾逐像素标量单线程阈值循环（分支不可向量化、不并行）。MT 下 box 部分切到物化路径（灰度行任务更小、开销占比更大），串行循环原封不动 → 总时间反超 ST。

**修复**：末尾循环 SIMD 化（见 §2.4）+ rayon 64 行块并行。MT 989 → 1157（vs OpenCV 1140，1.02×）。

**教训**：**函数内任何一段串行标量代码都会成为 MT 短板**（Amdahl）。落地时必须整条流水线并行化，不能只并行"主要部分"。

### 1.6 adaptive 尾循环：spin_pool 与 rayon 混用崩坏（重要）

**现象**：把 adaptive 尾循环从 rayon 改成 spin_pool（`run_blocks2`）后，MT 从 1157 崩到 258 MPix/s（比 ST 还慢 11×）。但 spin_pool 微基准（单独跑尾循环）却极快（6.6µs，95712 MPix/s）。

**根因**：adaptive 结构 = `box_filter`（rayon 两阶段）+ 尾循环。box 用 rayon 64 线程跑完后，**spin_pool 的 63 个常驻 worker 与 rayon 池 127 线程抢 64 核**。box 期间 spin worker 自旋烧 CPU，box 结束后 tail 的 spin 唤醒又慢。实测：`box(rayon) + run_blocks2` = 869µs，而 `run_blocks2` 单独 = 6.6µs（130× 差距）。换成 `box(rayon) + rayon tail` 也慢（877µs）——**box 的 rayon 任务结束后线程 park，后续任何并行区（无论 rayon 还是 spin）唤醒都慢**。

**结论**：**同一函数内不要混用 rayon 与 spin_pool 两个池**。adaptive 的 box 用 rayon，尾循环也必须用 rayon（同池，无跨池竞争）。spin_pool 只用于"纯单段"函数（threshold/to_grayscale，无前置 rayon 段）。

**最终方案**：尾循环用 rayon `par_chunks_mut`，**块大小 16 行**（不是 64 行）。微基准：chunk=16 → 110µs，chunk=32 → 316µs，chunk=64 → 326µs（16 行块在 MT 下 L2 局部性最好）。完整 adaptive MT 1157 → 1901（vs OpenCV 1140，1.67×）。

**教训**：块大小调参结论（§1.1 的"带宽型 64 行"）**不适用于带前置 rayon 段的尾循环**——尾循环块要小（16 行）以贴合 L2。同类结论必须结合函数整体结构判断，不能机械套用。

---

## 2. SIMD 内核工程规则

### 2.1 AVX512 intrinsics 陷阱（Rust stdarch 与 C 的差异）

| 陷阱 | 正确做法 |
|------|---------|
| `_mm512_i32gather_ps(index, src, scale)` 参数序与 AVX2 版**相反** | 仔细核对 stdarch 文档，不能凭 AVX2 记忆写 |
| `_mm512_cvtepu8_epi16` 输入是 `__m256i`（不是 `__m512i`） | 窄化指令输入取低半 |
| `_mm512_cvtepi32_epi16` 返回 `__m256i` | 输出类型是窄的 |
| 无 `_mm256_absdiff_epu8` | 用 `max - min`（无符号下 max≥min 恒成立） |
| 无 `_mm256_cvtepu32_epi32` | u32→i32 位相同，直接用 `_mm256_cvtepi32_epi32` 或不转 |
| 无掩码寄存器版 `_mm512_cmpgt_epi16`（只有 `_mask` 后缀版） | 用符号位技巧：`d > thr ⟺ d + (32767 - thr) ≥ 0`，即 `srai_epi16(add_epi16(d, bias), 15)` 得 0xFFFF/0x0000 掩码（前提 \|d - thr\| < 32768 无溢出） |
| `_mm512_mask_mov_epi8` 掩码参数是 `u64`（`_mm512_cmpgt_epi16_mask` 返回 `u32`） | 显式 `as u64`；取反用 `(k as u64) ^ 0xFFFF` |

### 2.2 unpack/pack 的 lane 序陷阱（AVX512 重灾区）

`_mm512_unpacklo/hi_epi8` 按 **128-bit lane** 分裂字节序：64 字节输入 unpack 后，lo/hi 两半各自内部是 lane 交错的，**不是**前 32 字节/后 32 字节。

**错误示范**（adaptive_thresh AVX512 首版）：unpack → `cmpgt_epi16_mask` 得 k_lo/k_hi → 以为 k_lo 对应 `[i, i+32)`、k_hi 对应 `[i+32, i+64)`，直接 mask_mov 存两段 → 输出错位（单测抓出）。

**正确做法**：i16 掩码（`__m512i`，0xFFFF/0x0000）经 `_mm512_packs_epi16(gt_lo, gt_hi)` 打包回 u8 掩码——**packs 与 unpack 的 lane 语义互逆**，恢复原始字节顺序，再统一 and/andnot + 单次 store。AVX2 的 `packs_epi16` + `permute4x64(0xD8)` 同理。

**通用规则**：unpack 出来的东西要回到字节序，必须走对应的 pack；掩码寄存器（k register）路径只适合"整批一起"的操作，不适合 unpack 后分半处理。

### 2.3 打包链（u32 → u16 → u8）

- `pack_u32_u16`：`packus_epi32` + `permute4x64(0xD8)`（修 lane 序）
- `pack_u16_to_u8`：`packus_epi16` + `permute4x64(0xD8)`
- **嵌套两次 `pack_u32_u16` 是错的**：u16 对被当 u32 读 → 饱和 65535 → 输出全 0。链式打包必须逐级用对指令。

### 2.4 有符号差值的 SIMD 化（adaptive_threshold）

`d = src - mean`，d ∈ [-255, 255]。**不能**用 `sub_sat_epu8`（无符号饱和截断负差）。正确路径：u8 unpack 到 u16（零扩展）→ `sub_epi16`（i16 语义，负差合法）→ 有符号比较。

比较用 bias 技巧或直接 `cmpgt_epi16`（AVX2 有掩码寄存器版返回 `__m256i`；AVX512 没有，见 §2.1）。结果 blend：Binary 用 `and(gt8, imax)`，BinaryInv 用 `andnot(gt8, imax)`——无分支。

### 2.5 interleave3_store / deinterleave3

- `deinterleave3`（avx2.rs）是 OpenCV `v_load_deinterleave` 的精确移植，按字节位置序返回三通道，调用方自行命名（to_grayscale: `(r,g,bl)`；bilateral: `(rb,rg,rr)`）
- `interleave3_store` 必须按 OpenCV `v_store_interleave`（intrin_avx.hpp:2752）实现：SH_B/SH_G/SH_R + M0/M1 blend + permute2x128 0x20/0x30/0x31。手写掩码版曾出错，用 C 测试程序逐字节验证后重写
- 两个都是 `pub(crate)`，AVX512 内核复用 AVX2 的这两个函数（32 像素粒度）

### 2.6 AVX2 内核禁用 AVX512VL intrinsic

`_mm256_cvtps_epu32` 是 AVX512VL 指令，在 `#[target_feature(enable = "avx2")]` 函数里能编译（构建全局启用了特性），但 AVX2-only CPU 上会崩。**已知未修**：bilateral AVX2 内核用了它（见 §7 遗留）。等价替代：`_mm256_cvtps_epi32` + 手动饱和。

### 2.7 `#[target_feature]` 内核必须 `#[inline(never)]`

LTO（fat）会把 target_feature 内核内联进普通函数，intrinsic 失去特性上下文被 outline 成函数调用（每条向量指令一次 call），性能崩塌 10-20 倍且**编译不报错、测试不失败**。提交前检查：`objdump -d target/release/examples/bench | grep "call.*core_arch"` 必须为空（std panic 路径除外）。

---

## 3. 算法对齐 OpenCV 的语义细节

### 3.1 bilateral_filter（对齐 bilateral_filter.simd.hpp）

- **radius**：`d <= 0 ? round(σs * 1.5) : d / 2`，最小 1
- **核形状**：圆形（`r > radius` 的角落跳过），不是方形
- **u8 路径**：color 权重 LUT 256·cn 项（cn=3 时索引 = 三通道 absdiff 之和，≤765）；**含中心像素**（wsum 初始 1，sum 初始 rval）；输出 round-half-even
- **f32 路径**：插值 expLUT（4096·cn bins，`scale_index = bins / ((max - min) * cn)`）；**不含中心**（`(sum + rval) / (wsum + 1)`）；NaN 语义：邻域 NaN 跳过、中心 NaN → `sum / wsum`
- **σ ≤ 1e-6**：直接拷贝
- 边界映射：Clamp=BORDER_REPLICATE、Reflect=BORDER_REFLECT_101、Constant=BORDER_CONSTANT

### 3.2 gaussian_blur（对齐 fixedSmoothInvoker）

- σ=0 时 3×3 核恰为 [1,2,1]/4 → `hline/vline_smooth_3n121` 特化（每像素 2 加法 + 1 移位）；5×5 为 [1,4,6,4,1]/16 → 14641 特化 + 融合单遍路径
- 定点 SHIFT=8，u8 → u16 中间行，ring buffer 流式（kh 行常驻 L1/L2）
- 14641 融合路径（w,h ≥ 5）：主体单遍 + 边框带 hline/vline（对齐 doCombined5N14641）

### 3.3 box_filter（对齐 FilterEngine + CV_16U 定点）

- 两阶段：行和（u8→u16）+ 列滑动累加 + 定点归约（SHIFT=23，`BoxFixed::from_area`）
- `kw*kh ≤ 256` 时 u16 中间不溢出（255×256=65280 < 65535）
- ST 流式（ring buffer 不物化 mid）；MT 物化 mid + 两阶段各自并行
- kh=3、C=3、kw∈{3,5} 有流式特化（三行滚动变量 + 源行索引预计算）

### 3.4 adaptive_threshold（对齐 thresh.cpp:1925-1965）

- MeanC：`boxFilter(BORDER_REPLICATE|BORDER_ISOLATED)`
- GaussianC：**f32 路径**（`src.convertTo(CV_32F)` → GaussianBlur f32 → convertTo u8 round-half-even），不是 u8 定点
- 尾循环：`tab[src - mean + 255]`，idelta = ceil(delta)（Binary）/ floor(delta)（BinaryInv）
- **已知未对齐**：我们的 GaussianC 走 u8 定点 gaussian，与 OpenCV f32 路径有 ±1 舍入差，被阈值放大（对拍 38.6dB，差异集中在 col 0）。修复方向：GaussianC 分支显式走 f32

### 3.5 to_grayscale

u8/u16：整数定点 `(R·9798 + G·19235 + B·3735 + 16384) >> 15`（round-half-up，对应 CV_DESCALE）；f32：`R·0.299 + G·0.587 + B·0.114`。

### 3.6 边界枚举映射

`Clamp` = BORDER_REPLICATE、`Reflect` = BORDER_REFLECT_101（**不含**边界像素，gfedcb|abcdefgh|hgfedcb）、`Constant` = BORDER_CONSTANT。注意 OpenCV 的 BORDER_REFLECT 是含边界的（fedcba|abcdefgh|hgfedcb），别搞混。

### 3.7 形态学（对齐 morph.dispatch.cpp / morph.simd.hpp）

- **矩形核可分离两段**：行 min/max + 列 min/max（`morph_separable_u8`）。非矩形核走通用 2D（`morph_2d`）。
- **多次迭代等价于扩大核**（morphOp:1010-1018）：`ekw = kw + (iterations-1)*(kw-1)`，`eax = ax*iterations`。矩形核一次调用即可，非矩形核逐次迭代。
- **常量边框语义**：erode 恒等值 255、dilate 恒等值 0。复合运算（open/close/gradient）两阶段 border 值不同，对拍仅测 Clamp/Reflect。
- **行阶段直接边框**：`morph_row_u8_direct` 省去 padded 行 memcpy 中转，内核内部处理首尾 `kw-1` 像素的越界采样。
- **kw=3 行内核特化**：`morph_row_u8_direct_k3` 编译期展开（无运行时 `for k in 1..kw` 循环），erode 293→179µs（0.37×→0.55×）。**关键教训：运行时 kw 的循环无法展开，是行阶段瓶颈。**
- **列阶段**：`morph_col_u8` 顺序访问 kh 行（`refs` 数组），无索引映射。
- **性能瓶颈**：行阶段（125µs）> 列阶段（60µs）。行阶段读 3 个重叠向量（1x 放大），列阶段读 3 行（3x 放大）。
- **尝试过但回归的方案**：
  - **col2（2 行一次）**：读 4 行/2 输出（1.5x 少读），但 `refs` 数组构造 + `while` 循环开销 > 读减少收益，4480 vs 5760（回归）。
  - **fused 3x3（单遍融合）**：省中间缓冲（6MB→4MB 流量），但 9 次 load/64 像素（3 行 × 3 偏移）远超可分离的 6 次，2295 vs 5760（严重回归）。**结论：形态学 load 数主导，可分离两段更优。**
- **流式环形缓冲（2026-09-30 最终方案，ST 1.06-1.67×）**：`morph_streaming_u8` 家族——kh 行环形缓冲（L1 常驻），行滤波结果立即被列滤波消费，中间图不物化（避免列阶段从 L3 读冷数据）。erode/dilate 11285/11261 MPix/s（此前物化路径 5729/5603）。
- **两级流水线（open/close）边框语义（重要教训）**：OpenCV `morphologyEx` = 两次独立 `erode`/`dilate` 调用，**第二级垂直边框作用于第一级输出中间图 E**（Clamp: E(-1)=E(0)；Reflect101: E(-1)=E(1)；Constant→常量行），不是把第一级窗口延伸出图。错误实现导致对拍 19.5dB。
- **环形缓冲行号-槽位约定**：槽 s 存行 s（`v.rem_euclid(kh)`），预填行范围必须按此推导（open/close 预填 mid1 行 {-1,0}，主循环 e∈[0,h) 先滤行 e+1）。行号偏移错 1 会导致输出整体下移一行（3x3 小图单测可捕获）。

### 3.8 梯度（对齐 deriv.cpp / filter.simd.hpp / mathfuncs_core.simd.hpp）

- **Sobel/Scharr 8U→16S bit-exact 整数路径**：`createSeparableLinearFilter` KERNEL_INTEGER 分支（bits=0，CV_32S 中间缓冲）。行 u8→i32（`SymmRowSmallVec_8u32s`），列 i32→i16 饱和（`SymmColumnSmallVec_32s16s`）。
- **核存储约定**：`[k[-1], k[0], k[+1]]`，**反对称核有效系数是 k[+1]=kx[2] 不是 kx[0]**（曾致符号反转）。dx=0→kx=[1,2,1]、dy=1→ky=[-1,0,1]；Scharr order0=[3,10,3]。
- **[1,2,1] 专用加法路径**（对齐 OpenCV）：行核 `2c+l+r`、列核 `S0+S2+2*S1` 均无乘法（`SymmRowSmallVec_8u32s` 的 `kx[0]==2&&kx[1]==1` 分支）。
- **Laplacian ksize=3 是对角核** `{2,0,2;0,-8,0;2,0,2}`（deriv.cpp:721 K[1]），不是十字。ksize=1 十字 `{0,1,0;1,-4,1;0,1,0}`。
- **Constant 边框对角邻居**：垂直越界行的对角邻居 y 也越界 → 常量（不是中心行的 l/r！曾致 lap k3 RGB 30dB）。水平越界同理。
- **流式 ring 两段（ST 关键，2410→6345 MPix/s）**：mid 滚动 3 行 ring（L1 常驻），行滤波领先列滤波 1 行。物化全量 mid（4.2MB）会让列阶段从 L3 读冷数据。MT 用 16 行块独立流水线（块内 mid=rows+2 行，块间无依赖）。
- **带宽型 MT 块大小**：laplacian 64 线程 16 行块实测 1.1K MPix/s（总线争抢），256 行块 3.7K。**块太小引发内存总线争抢，带宽型内核要大块**。
- **magnitude/phase/cart_to_polar**：`sqrt(fma(x,x,y*y))`；atan 多项式 P1/P3/P5/P7（mathfuncs_core.simd.hpp:80 `v_atan_f32`，度数系数乘 180/π），c=min/max+f64::EPSILON as f32，FMA 链；象限修正 ax>=ay?a:90-a、x<0?180-a、y<0?360-a。f32 输入走零转换快路径 + 256K 元素块并行。
- **输出缓冲 alloc_uninit**：`Image::new` 零初始化 2MB 在热路径可见（sobel 5084→2080），SIMD 全量覆写场景必须用未初始化分配。

### 3.9 Canny（对齐 canny.cpp）

- **数据流**：Sobel 16S（REPLICATE）→ 幅度（L1 `|dx|+|dy|` 或 L2 `dx²+dy²`）→ NMS+双阈值（map 0=候选/1=非边缘/2=边缘）→ 滞后洪泛 → finalPass（map==2→255）。
- **阈值语义**：`low>high` 先交换；L2 时 `min(32767,t)²`（先 clamp 再平方）再 floor。**aperture=5 无缩放**（阈值/16 和 Sobel scale=1/16 都只在 aperture=7，曾搞混致 5.7dB）。OpenCV 5.0 的 aperture=-1（Scharr）因 `-1 & (1<<31)` 被当 L2GRADIENT 标志位实际不可用。
- **NMS 定点角度**：TG22=13573，`y=|dy|<<15` vs `x*TG22`（水平）、`tg67x=tg22x+(x<<16)`（垂直）、否则对角 `s=(xs^ys)<0?-1:1` 比较 `mag_p[k+1-s]`/`mag_n[k+1+s]`。前向邻居 `>`、后向 `>=`。
- **map 布局**：`(h+2) × align(w+33,32)`，pmap=行+32 偏移；行 0/h+1 全 1 哨兵；NMS 内核写每行 [1,w] + 首尾哨兵（alloc_uninit 只预填哨兵行）。
- **mag ring 尾行清零（曾致底行 4 像素差异）**：处理最后输出行 y=h-1 时 n 槽必须显式清 0（OpenCV 块尾 memset），否则残留行 h-3 数据。
- **块内 Sobel 的 mag ring 必须 4 槽（曾致 262K vs 161K 边缘，PSNR 7.5dB）**：NMS 行 y 需 mag [y-1, y, y+1]，且 `col_stage(y+2)` 写 mag 行 y+2。3 槽时 `(y+2)%3 == (y-1)%3`，col_stage 覆盖 NMS 的前一行 → 全图错乱。改 4 槽（`%4`）后 dx/dy ring 仍 3 槽（`%3`，NMS 只需当前行 dx/dy），两者索引分开。
- **首/末块 mag 边界行清零（对齐 OpenCV ring 初始化）**：首块 mag 行 -1 应为全 0（OpenCV 首块 `memset(_mag_n-1,0,mapstep)`），末块 mag 行 h 应为全 0（OpenCV 块尾 memset），而非用 clamp 源行算出的 mag 行 -1/h。曾用 clamp 源行 [0,0,0] 算 mag 行 -1，与 OpenCV 全 0 不同致首行 NMS 错。
- **IMREAD_GRAYSCALE ≠ BGR2GRAY**（曾致 7.7dB）：libpng 灰度解码 vs RGB 加权，50% 像素不同。对拍 C++ 侧必须 `imread(COLOR)` + `cvtColor(BGR2GRAY)`。
- **NMS 内核必须整体下沉 simd_kernels**（114→193 MPix/s）：候选函数 `nms_one` 裸指针 + `#[inline(always)]`（切片版有 bounds check + 未内联 call，16 万候选 × ~100 cycles）。批预筛 AVX512 64/AVX2 32 元素 + trailing_zeros 逐候选（对齐 v_scan_forward）。
- **finalPass 行块并行**：逐行 par_chunks_exact_mut(w) 任务粒度灾难（1.33ms），16 行块后 0.65ms。
- **MT 结构**：块内 Sobel（对齐 OpenCV parallelCanny，每块只算自己行+2 halo，不物化全图 dx/dy）+ 行块 NMS/局部洪泛（块边界行峰值收集）+ 串行全局洪泛。**rcv MT 0.65× 的主因**：dx/dy 全图 4MB mmap 分配每迭代缺页（OpenCV 块内 64KB arena 复用）+ 全局洪泛串行段；需内存池重构（遗留）。
- **sobel5**（aperture=5）：5-tap 核 [1,4,6,4,1]×[-1,-2,0,2,1]，标量整数路径（行 u8→i32、列 i32→i16 饱和），边框采样用**绝对坐标** interpolate（曾传相对偏移致右缘 2 列 36dB）。**块内 Sobel 改造后 aperture=5 暂不支持**（块内只实现 3×3/Scharr 核，Sobel5 走 3×3 占位致对拍 8.7dB，遗留）。

### 3.10 find_contours 家族（对齐 contours_new.cpp / contours_truco.cpp / contours_approx.cpp / geometry/approx.cpp）

- **结构**：RETR_LIST 无 hierarchy → TRU 并行快路径（tru.rs）；带 hierarchy 或其他模式 → Suzuki-Abe 串行（suzuki.rs）。RETR_LIST 带 hierarchy 请求时走 Suzuki 并填 next/prev 兄弟链（对齐 OpenCV）。
- **Suzuki 关键语义**：schar 路径标记 setRightFlag=`nbd|0x80`、setNewFlag=`nbd`，isVal 判定 `img[i3]==1`（MASK8_BLACK，非 int 路径的 `img[i3]==img[i0]&0x7F`）；轮廓点坐标 = padded - 1（origin 含 offset(-1,-1)）；**trace_contour 的 s 即 while 循环变量**（找到 i4 后必须 `s = ss` 再 `(s+4)&7` 推进，漏掉致对角线图案两像素震荡死循环）；fetch_contour 有 `s &= 7`（与 trace 不同）。
- **TC89**：跟踪阶段 approx_method1=CHAIN_CODE（收链码），make_contour 时 approximateChainTC89 转点；**prev_origin 是 origin+offset（含 -1,-1）**，转换必须用原图坐标系（曾致全图 +1 偏移）。approx_method1 用 `Option<ContourApproxMethod>`（None=CHAIN_CODE 哨兵）。
- **approxPolyDP**：farthest 必须循环内追踪（right_start=pos 当前点下标）；内层循环后 pos 绕回本轮起点；闭合初始化栈 `new_right_start=(right_slice_start+slice_start)%count`，push `(new_right_start, slice_start)` 和 `(slice_start, new_right_start)`。
- **TRU 性能三连**（ST 698→1897 MPix/s）：① 跟踪内核预计算 16 项邻居 offsets 数组（消除热循环乘法，1503→1045µs，最大头）；② pad+二值化改行 copy + SIMD threshold_u8（107→68µs）；③ 轮廓收集 mem::take 替代 clone。**条带数钳制 min(threads,8)**：64 条带（每条 16 行）在共享机器上争抢 + 合并开销负收益（MT 812 MPix/s），8 条带 MT 1841。
- **TRU 合并**：首行只产生外轮廓（内轮廓要求 r>row_start），故 acc_t1 头部 ke 个外轮廓索引连续 0..ke，drain(0..ke) 即可（曾用逐个 remove O(ke·n)）。
- **contourArea(3×3 方块 8 点边界环) = 4**（OpenCV 实测），非 9。
- **对拍**：7 图 × 4 模式 × 4 方法 × 2 hierarchy = 224 组合全 EXACT（i32 LE 流：`[n][len][x y...][hier_n][next prev fc parent]*n`）。

### 3.11 绘制家族（对齐 drawing.cpp）

- **结构**：`src/imgproc/drawing.rs` 单文件，公开 API：line/rectangle/circle/ellipse/polylines/fill_convex_poly/fill_poly/draw_contours（各带 `_rgb` 变体，灰度版 `[T;1]` 泛型 + RGB 版 `[T;3]`）。Canvas trait 用 `bytemuck::cast_slice_mut`（safe）实现，无 unsafe。
- **line_raster（LineIterator）**：必须实现 leftToRight=true（dx<0 时交换端点），步进 `mask = err<0 ? -1 : 0; err += minusDelta + (plusDelta & mask)`。LINE_4=4 ≠ 1：Connected4 + shift>0 走 line_fixed（Line2 定点）。
- **clip_line 引用语义（大坑）**：OpenCV 用 C++ 引用 `int64 &x1 = pt1.x`，第一段 clip y 修改 x1 后，第二段 clip x 的 `(x2-x1)` 用**新值**。Rust 快照局部变量会差 1 像素（gray.png 20×13 图外顶点对角线整体偏 1）。用宏模拟引用（`macro_rules! x1 { () => { pt1.x } }`）。
- **fill_edge_collection 哨兵**：`PolyEdge { y0: i32::MAX, next: -1 }`——next 必须 -1（OpenCV `tmp.next = 0` 即 null），`..Default::default()` 的 next=0 会被当活动边 → 死循环。冒泡排序需 `last >= 0` 保护。
- **fill_convex_poly 边推进**：OpenCV `for(; edges-- > 0; )` 判断后减，edges 可到 **-1**（`if (edges < 0) break` 才能触发）。Rust `while edges > 0 { edges -= 1; }` 最低到 0，永不 break → 底边行多画（gradient.png 64×48 y=45 行 53 字节差）。修复：`while { let cond = edges > 0; edges -= 1; cond }`。
- **circle mask**：`(err <= 0) - 1` → err>0 时 -1，err<=0 时 0（与直觉相反）。
- **sincos 参数序**：`sincos(angle, alpha, beta)` 中 **alpha=cos, beta=sin**（OpenCV 参数名误导）。SIN_TABLE 451 项；ellipse2Poly：x=axes.w·SinTable[450-a]（cos），y=axes.h·SinTable[a]（sin）。
- **cvRound 是 ties-to-even**，Rust `.round()` 是 away-from-zero——定点转换潜在隐患（当前测试未触发）。
- **drawContours**：thickness<0 → fillPoly；thickness>0 → 每轮廓 PolyLine(closed)。
- **对拍**：5 图（bench/gray/gradient/non_square/single_col）× 8 shape × 3 lineType × 3 thickness × 2 shift = **690 组合全 EXACT**（ref 侧 polylines thick=-1 是 OpenCV 断言，跳过）。**小图（图外顶点）是 clip 路径的必测项**——大图顶点全在图内测不出 clip bug。

### 3.12 按位逻辑（对齐 arithm.cpp:378-402）

- **语义**：OpenCV bitwise_and/or/xor/not 按**底层字节位模式**运算（与像素类型无关，u8/u16/f32 均同）。Rust 侧 `bytemuck::cast_slice` 转字节域后 SIMD 处理，任意 `T: Pixel + Pod` 泛型。
- **大坑**：`n` 必须是**字节数**（`cast_slice(a.data()).len()`），不是元素数——u16/f32 时差 2/4 倍，后半输出未初始化（new_uninit）读垃圾。
- **内核**：avx2 32B/迭代、avx512 64B/迭代（and/or/xor/not 四个），not 用 `xor -1`。标量 fallback。
- **对拍**：5 图 × 4 op = 20 组合全 EXACT（第二操作数 = 水平翻转）。
- **性能**：纯带宽极限（1MB 图 3 字节流/像素）。ST：and/or/xor ~10.8-11.2K vs OpenCV ~11.0K = 1.00×（噪声内持平）；not 22-23K vs 24-25K = 0.9×。**not 内核优化（2026-10-09）**：`vpternlogd $0x01`（三操作数全 a，真值表索引 0→1/7→0 即 ~a）替代 load+xor(-1)+store，纯内核 44→77 GB/s（+74%，超 OpenCV 52 GB/s）。**踩坑**：imm=0xFF 是全 1 不是取反（真值表全 1 = 常量 1），曾致输出全 0xFF——对拍立即抓出。API 层 not 仍 0.9×：`new_uninit` 每次走 mmap 缺页 vs OpenCV fastMalloc 缓存复用（Image 持有 Vec 所有权，TLS 池化需改 API，遗留）。MT 双方均无扩展（带宽饱和，并行无益）。

### 3.13 轮廓几何分析 + 连通域（对齐 modules/geometry + connectedcomponents.cpp）

- **arc_length**：点转 f32 后累加（对齐 OpenCV float 中间精度），f64 累加器。
- **convex_hull（Sklansky）**：① 指针排序 (x, y, 原下标) 三元组（对齐 CHullCmpPoints 的指针序）；② 四段 Sklansky 扫描（tl/tr 上半 + bl/br 下半，clockwise=false 时 swap tl↔tr）；③ **输出前循环移位规范化**（convhull.cpp:296-335，把下标序列旋转成升/降序）——**漏掉此段输出是同一环的不同起点**（对拍 644/692 差异全为旋转关系）；④ 共线检测（stop_idx/check_idx）镜像上下半。
- **min_area_rect**：convexHull → rotating calipers（4 卡尺边，firstVecIsRight 选主边）。buffer[0]/[5] 用 f32 槽存 int 位模式（对齐 C union）。angle ∈ [-90, 0)。
- **min_enclosing_circle**：Welzl 三层 + **cv::RNG 复刻**（MWC：state = lo·4164903690 + hi；seed = count ^ x0 ^ (y0<<8) ^ (xn<<16) ^ (yn<<24)，Cv32suf 位模式）。count>10 才洗牌。EPS=1e-4。
- **connected_components（SAUF/Wu + 条带并行）**：两遍扫描 + union-find（findRoot/setRoot/set_union/flattenL 逐行对齐）。标签上界 (w·h+1)/2+1。**背景标签 0 也有 stats**（对齐 CCStatsOp——背景像素的包围盒/面积/质心，非零）。Rust 侧 prev/cur 行用 split_at_mut 借用。**MT 路径对齐 LabelingWuParallel**：条带并行第一遍（每条带独立标签空间，stripeFirstLabel = r0/2·((w+1)/2)+1，条件用 `r > limit_line`）→ 边界行 mergeLabels → flattenLParallel → 并行第二遍。**条带标签区间按构造不相交**（stripeFirstLabel 保证），p 数组按条带边界 split_at_mut 成不相交切片后 par_iter_mut（无 unsafe）。条带数上限 16（更多时 merge/flatten 串行段与缓存争用反噬）。
- **对拍**：geometry 6 op × 5 图 = 30 组合全 EXACT（arc_length/convex_hull/is_convex/min_area_rect/min_enclosing_circle/contour_area）；cc 5 图 × 2 连通 = 10 组合全 EXACT（**C++ 侧固定 CCL_SAUF**——OpenCV 默认 Spaghetti 的标签编号顺序与 SAUF 不同，划分相同）。
- **性能**（ST，MPix/s）：arc_length 6647 vs 4833 = **1.38×**；convex_hull 231 vs 194 = 1.19×；min_area_rect 225 vs 191 = 1.18×；min_enclosing_circle 1043 vs 567 = **1.84×**；connected_components 824 vs 777 = **1.06×**（优化链见下）。

- **cc 优化链（552 → 824 MPix/s，2026-10-10）**：① flatten 只遍历 [1, lunique)（OpenCV 语义；全量 plength 遍历 355→66µs）；② labels 用 new_uninit（pass1 全覆盖写入）+ p 用 alloc_uninit 只清 p[0]（标签 [1,lunique) 连续分配全被 pass1 写自指，未用槽不被 flatten 触碰）——省 6MB 清零 ~218µs；③ pass2 重写内核 `cc_relabel_inplace` 进 simd_kernels（裸指针 8 展开，消除每元素边界检查 + movslq 依赖链，600→277µs = C++ 同款 276µs）；④ MT：条带数钳制 16 + flatten 按条带实际标签数（对齐 chunksSizeAndLabels）+ pass2 每条带一个内核调用。**教训**：Rust checked 索引的 cmp+ja 与 movslq 在依赖链密集的 gather 循环里代价 ~2×（370 vs 277µs），此类内核必须进 simd_kernels；`Vec::clone` 不是 memcpy（267µs vs 130µs），profiling 对照必须含相同前缀操作。
- **cc 性能**（ST，MPix/s）：824 vs 777 = **1.06×**；MT 645 vs 325 = **1.98×**（共享负载下 OpenCV MT 从 775 跌到 325，我们 64 线程 rayon 池同样受争用但条带并行仍 2× 超越）。

---

## 4. 对拍与 bench 方法论

### 4.1 对拍

- 入口：`scripts/compare.sh <func> <input.png> [args...]`（编译 ref → 跑 ref → 跑 rust → psnr.py）
- 参数格式（易错，每次新增函数要核对 examples/compare.rs 与 compare_opencv5.cpp 的 argv 解析）：
  - `bilateral_filter <d> <σc> <σs> <border 0/1/2> [gray 0|1]`
  - `box_filter <ksize> <normalize 0|1> <border> [gray]`（**不是** anchor！曾把 normalize 位当 anchor 传错）
  - `gaussian_blur <ksize> <sigma> <border> [gray]`
  - `adaptive_threshold <maxValue> <method 0=mean 1=gaussian> <type 0/1> <blockSize> <delta>`
  - `sobel|scharr|laplacian <dx> <ksize> <scale> <border> [gray]`（DTYPE=i16；sobel ksize=5 走 sobel5）
  - `magnitude|phase|cart_to_polar <dx> <ksize> <scale> <border> [gray]`（DTYPE=f32，输入由 sobel 16S 转 f32）
  - `canny <low> <high> <aperture 3|5|-1> <l2 0|1>`（灰度输入，C++ 侧 BGR2GRAY）
  - `DTYPE=<u8|u16|i16|f32> scripts/compare.sh ...` 指定 psnr 像素类型（默认 u8）
- **对拍失败先自证**：写临时 example 复现"旧标量逻辑 vs 新 SIMD"对比（mismatched pixels: 0 证明内核正确），再查 compare 参数传递——本次 box_filter 39dB 就是参数传错 + par_chunks_exact_mut 真 bug 混在一起
- PSNR=inf 才是目标；60dB 是底线；30-40dB 通常意味着边界行/列或舍入差

### 4.2 bench

- rcv：`cargo run --release --example bench [--threads]`（best-of-5）
- OpenCV：`LD_LIBRARY_PATH=/opt/opencv5/lib ./compare/opencv5_bench compare/inputs/bench.png [--threads]`
- **共享机器噪声大**：内存带宽型用例波动可达 3×，必须 best-of-5 取最小值；跑之前 `uptime` 确认 loadavg 正常，杀掉失控进程（`ps aux --sort=-%cpu`）
- MT bench 挂起/超慢时，先查是否有失控 spin worker（见 §1.1 验证方法）
- 示例内单线程：`rayon::ThreadPoolBuilder::new().num_threads(1).build_global()`（median_isa_bench 的经验）

---

## 5. API 风格决策

2026-09-29 定稿（DESIGN.md 铁律 3/5 + §3 第 9 条）：

- **纯计算函数**：opencv-python 风格，`fn xxx<T: Pixel, const C: usize>(src: &Image<T, C>, ...) -> Image<T, C>`，返回新分配，禁止 `&mut dst` 输出参数
- **IO 函数**（imread/imwrite）：`Result<T, Error>`
- **非法参数**（偶数核、空图）：panic + `# Panics` 文档（对齐 std 惯例）
- 例外：实测分配热点时可补 `*_in_place` 变体
- 现有代码（threshold/gaussian_blur/...）本来就是返回 Image，文档与代码已对齐
- **待办**：`src/core/error.rs` 的 `DimensionMismatch`/`UnsupportedChannels`/`OutOfRange`/`EmptyInput` 变体按新契约应裁剪（纯计算不再用）

---

## 6. 当前性能基线（2026-09-29，64 核 MT / 单线程）

| 函数 | rcv ST | rcv MT | OpenCV ST | OpenCV MT | MT 比值 |
|---|---|---|---|---|---|
| to_grayscale | 4057 | 52051 | 2705 | 32943 | 1.58× |
| threshold_binary | 23225 | 232327 | 23107 | 54472 | 4.27× |
| adaptive_mean_3 | 2898 | 1901 | 1123 | 1140 | 1.67× |
| box_filter_3x3 | 1770 | 5164 | 1638 | 1632 | 3.16× |
| gaussian_blur_3x3 | 1574 | 11671 | 1450 | 10781 | 1.08× |
| median_blur_3x3 | 1886 | 13916 | 1740 | 1740 | 8.00× |
| bilateral_d9 | 41 | 632 | 11 | 131 | 4.82× |
| erode_3x3 | 11285 | 10227 | 10579 | 10008 | 1.02× |
| dilate_3x3 | 11261 | 11160 | 10305 | 10104 | 1.10× |
| morph_open_3x3 | 6323 | 6157 | 5604 | 5534 | 1.11× |
| morph_close_3x3 | 6335 | 6193 | 5714 | 5553 | 1.12× |
| morph_gradient_3x3 | 6407 | 6266 | 3823 | 3897 | 1.61× |
| sobel_dx_3x3 | 6358 | 12114 | 3697 | 3695 | 3.28× |
| scharr_dx_3x3 | 5341 | 11065 | 3366 | 3362 | 3.29× |
| laplacian_3x3 | 6525 | 3800 | 2898 | 2868 | 1.32× |

**2026-10-08 梯度 MT 流式 ring 重写**（sobel/scharr 共用 `deriv_sep_u8_to_i16`）：MT 块内从"物化全块 mid（rows+2 行 i32）"改为 3 行流式 ring（与 ST 路径同构，mid 常驻 L1）。64 核下全块 mid 物化引发 DRAM/NUMA 争抢（实测线程扩展：4 线程 14.8K → 64 线程 7.6K 倒挂），流式后 MT64 7.6K→9.5K。MIN_ROWS 16→256（16 行块 64 核总线争抢 1.1GB/s 级退化，256 行块 1024 行图切 4 块最优）。重写后 MT：sobel 6446-6458（1.75×）、scharr 5389-5393（1.59×）、laplacian 6545-6647（2.15×）vs OpenCV MT 3691/3368/2947。带宽极限分析：sobel 每迭代 3MB DRAM 流量（src 1MB + dst 2MB），单 socket ~30GB/s → 64 线程理论上限 ~10K MPix/s，实测 9.5K 吻合；4 线程 21K 为 L3 命中（src+dst 3MB < L3）。| magnitude | 2427 | 4316 | 2476 | 2485 | 1.74× |
| phase | 2352 | 4784 | 1899 | 1907 | 2.51× |
| cart_to_polar | 368 | 3825 | 359 | 357 | 10.7× |
| canny_50_100 | 199 | 947 | 219 | 1467 | 0.65× |
| find_contours_list | 1897 | 1841 | 1411 | 943-1066 | 1.7-1.9× |
| find_contours_tree | 384 | 383 | 354-371 | 374-376 | 1.02× |
| draw_line | 394201 | - | 394417 | - | 1.00× |
| draw_rectangle | 133837 | - | 120038 | - | 1.11× |
| draw_circle | 53564 | - | 52972 | - | 1.01× |
| draw_ellipse | 234359 | - | 230350 | - | 1.02× |
| draw_polylines | 159389 | - | 153685 | - | 1.04× |
| draw_fill_poly | 245832 | - | 245441 | - | 1.00× |
| draw_contours | 51.7 | - | 46.0 | - | 1.12× |
| bitwise_and | 10964 | 10986 | 10933 | 11141 | 1.00× |
| bitwise_or | 10969 | 11104 | 10929 | 11134 | 1.00× |
| bitwise_xor | 10840 | - | 11000 | - | 0.98× |
| bitwise_not | 22500 | - | 24300 | - | 0.93× |
| arc_length | 6647 | - | 4833 | - | 1.38× |
| convex_hull | 231 | - | 194 | - | 1.19× |
| min_area_rect | 225 | - | 191 | - | 1.18× |
| min_enclosing_circle | 1043 | - | 567 | - | 1.84× |
| connected_components | 824 | 645 | 777 | 325 | 1.06× |

（MPix/s；bilateral_d9 = d=9, 75/75, Reflect；median MT 数字波动大，5.7-8.4× 区间；形态学 ST 比值 1.06-1.67× 全部 ≥1.0×，2026-09-30 流式环形缓冲重写后数据，机器 load 6-16 噪声下 MT 波动 ±10%；梯度 2026-09-30 数据，sobel/scharr MT 波动 10-14K 区间，magnitude ST 0.98× 为纯带宽极限（单核 28GB/s vs OpenCV 29GB/s），MT 1.74× 补偿；canny 2026-10-06 数据，ST 0.91×，MT 0.65× 为 dx/dy 全图 mmap 分配缺页 + 全局洪泛串行段（OpenCV 块内小分配），内存池重构为遗留优化项。**2026-10-08 块内 Sobel + mag ring 4 槽修复后**：对拍 PSNR=inf，ST 典型 1200-1750（最佳 1948）vs OpenCV 1300-1700（最佳 1743）≈1.0×；MT 典型 1200-1650（最佳 2233）vs OpenCV 1400-1700（最佳 1801）≈0.95-1.2×，ST/MT 均达 OpenCV 水平，机器 load 8-50 剧烈震荡下多轮采样。**2026-10-08 二轮优化（NMS alignr + map TLS 池）**：AVX512 NMS 每批 mag loads 9→6（2×512 位 load + alignr 提取 [j+1,j+17)/[j+2,j+18) 列偏移视图，主循环条件收紧 j+32≤w+2）；map 1MB 分配进 CANNY_TLS 池（跨迭代迭代免 mmap 缺页）。同进程 50 次采样 min：ST 406µs（2582 MPix/s）vs OpenCV 空窗 ~560µs（1850）≈1.4×；MT 490µs（2142）vs OpenCV ~690µs（1500）≈1.4×。负载 20-50 时官方 bench 数字不可用，以 min 采样为准。**find_contours 2026-10-09 数据**：同进程 50 次 min 采样，list ST 553µs（1897）vs OpenCV 725µs（1411）=1.34×，list MT 570µs（1841）vs 1066µs（943-1066）=1.7-1.9×；tree（Suzuki 串行，OpenCV 同为串行）ST/MT 均 ~384 vs ~370 = 1.03×。TRU 优化链：offsets 预计算 + SIMD pad/二值化 + mem::take + 条带钳制 8。**绘制家族 2026-10-09 数据**：ST 全部 ≥1.0×（line 1.00×/rectangle 1.11×/circle 1.01×/ellipse 1.02×/polylines 1.04×/fill_poly 1.00×/draw_contours 1.12×）——绘制是标量 Bresenham/扫描线算法，双方同构，1.0× 即达上限；MPix/s 数字巨大是因为耗时与像素数无关（一条线 ~µs 级），仅作回归监控）

---

## 7. 遗留问题清单

| # | 问题 | 影响 | 优先级 |
|---|------|------|--------|
| 1 | adaptive GaussianC 走 u8 定点，OpenCV 走 f32 + BORDER_ISOLATED | 对拍 38.6dB（col 0 的 144 像素差） | 中（正确性） |
| 2 | bilateral AVX2 内核用了 `_mm256_cvtps_epu32`（AVX512VL） | AVX2-only CPU 可移植性 | 低（当前机器 Avx512） |
| 3 | bilateral f32 路径仅标量 + rayon（无 SIMD） | f32 吞吐低于 u8 | 低 |
| 4 | error.rs 多余变体未按新 API 契约裁剪 | 文档/代码不一致 | 低 |
| 5 | adaptive_mean MT 1.67×（ST 2.58× 但 MT 扩展比仅 0.66×） | box 灰度 MT 物化路径 + 尾循环块大小 | 低（已达标，可再优化） |
| 6 | ~~形态学 ST 未达 1.0×~~ 已解决（2026-09-30）：流式环形缓冲重写（`morph_streaming_u8` 家族），ST 1.06-1.67×、MT 1.02-1.61× 全部达标。教训：两级流水线（open/close）第二级垂直边框作用于**第一级输出中间图 E**（OpenCV morphologyEx 两次独立调用各自边框），不是把第一级窗口延伸出图；行号-槽位映射（`v.rem_euclid(kh)`）与预填行范围必须按"槽 s 存行 s"约定推导 | 已关闭 | - |
| 7 | ~~find_contours TC89 未对拍~~ 已解决（2026-10-09）：approx_method1 改 `Option`（None=CHAIN_CODE 哨兵）+ TC89 转换用原图坐标系 origin-(1,1)。224 组合全 EXACT。性能：TRU offsets 预计算 + SIMD pad + 条带钳制 8 后 list ST 1.34× / MT 1.7-1.9×，tree 1.03×（双方均串行 Suzuki） | 已关闭 | - |
| 8 | ~~绘制家族对拍~~ 已解决（2026-10-09）：clip_line 引用语义（宏模拟）+ fill_edge_collection 哨兵 next=-1 + fill_convex_poly edges 可到 -1。690 组合全 EXACT（5 图 × 8 shape × 3 lt × 3 thick × 2 shift）。教训：**小图（顶点在图外）是 clip 路径必测项**，大图全在图内测不出 | 已关闭 | - |
| 9 | imwrite JPEG 编码 0.15×（23.8 vs 161.7 MPix/s）：`image` crate 纯 Rust 标量编码器 vs OpenCV libjpeg-turbo SIMD。PNG 写 1.61×/读 1.32× 已领先。优化方向：turbojpeg-sys FFI 后端（重依赖）或等待 image crate SIMD 化 | 低（IO 非热路径） |
| 10 | imwrite PNG 灰度 0.55×（71.6 vs 129.8）：rcv 灰度/RGB 同速，OpenCV 灰度路径有独立优化 | 低 |
| 11 | ~~connected_components 0.71×~~ 已解决（2026-10-10）：flatten 只到 lunique + labels/p 未初始化分配（pass1 全覆盖）+ pass2 内核进 simd_kernels（裸指针 8 展开）+ MT 条带并行（对齐 LabelingWuParallel，条带钳 16）。ST 824 vs 777 = 1.06×，MT 645 vs 325 = 1.98×。教训：① checked 索引 + movslq 在 gather 循环代价 ~2×，此类内核必须进 simd_kernels；② `Vec::clone` ≠ memcpy（267 vs 130µs），profiling 对照需含相同前缀；③ MT profiling 的"原地幂等重复"会因值域收敛高估 C++（p[p[x]] 二次起索引全 L1），必须 fresh 输入对照 | 已关闭 | - |

---

## 8. 后续阶段的检查清单（模板）

新函数落地时按此核对（复制本节到 PR/commit 描述）：

- [ ] 精读 OpenCV 源码（`*.simd.hpp` / `*.dispatch.cpp`），列出舍入/边界/特化路径
- [ ] AVX2 + AVX512 内核同时落地，`#[inline(never)]` + `#[target_feature]`
- [ ] 标量 vs AVX2 vs AVX512 三路径交叉验证单测（含跨 32/64 边界的长度）
- [ ] rayon 行块并行（块大小参考 §1：带宽型 64 行，计算型 16 行）
- [ ] `par_chunks_mut`（不是 exact，除非长度必整除）
- [ ] 对拍 PSNR=inf（多参数组合：ksize × border × gray）
- [ ] bench ST + MT，与 OpenCV 对比 ≥1.0×
- [ ] `scripts/check_unsafe.sh` + clippy 零新增警告
- [ ] 对拍 PSNR=inf（多参数组合：ksize × border × gray）
- [ ] bench ST + MT，与 OpenCV 对比 ≥1.0×
- [ ] `scripts/check_unsafe.sh` + clippy 零新增警告
- [ ] `objdump -d ... | grep "call.*core_arch"` 为空
- [ ] 踩到新坑 → 追加到本文件对应章节

形态学专项：
- [ ] 矩形核走可分离两段，非矩形核走通用 2D
- [ ] 多次迭代等价于扩大核（`ekw = kw + (iterations-1)*(kw-1)`）
- [ ] 行阶段 kw=3 特化（`morph_row_u8_direct_k3`，编译期展开运行时循环）
- [ ] 常量边框：erode 恒等 255、dilate 恒等 0；复合运算两阶段 border 值不同
- [ ] 性能：行阶段读 3 重叠向量（1x 放大）为瓶颈；col2/fused 均回归，勿用

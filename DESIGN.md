
# Rust 工业视觉核心库设计纲领

> **定位**：不是翻译 OpenCV，而是用 Rust 的类型系统、所有权和零成本抽象，重新设计一个更快、更安全的工业视觉核心库。
>
> **唯一对标基线**：OpenCV 5.0（安装于 `/opt/opencv5`）。所有已落地算法，同机、同图、同参数对比，吞吐量不得低于 OpenCV5，计算密集型函数应力争超越。
>
> **opencv源码路径**:`/workspace/programs/c++/opencv-5.0.0/`,实现过程中参考opencv5的源码，可以在此源码基础上优化。
>
> **通道顺序**：本库公开 API 一律 **RGB**（Red-Green-Blue），与 OpenCV 的 BGR 相反。任何与 OpenCV 对拍的地方，仅在 `compare/` 层做一次显式通道翻转，库内不做 BGR 兼容。
>
> **实施原则**：AVX2 手写内核为主力优化路径，AVX512 为可选加速路径，标量迭代器实现作为正确性基准和非 x86 平台兜底。不预建空模块，不提前引入预留依赖。

---

## 1. 架构铁律（不可违反）

| # | 铁律 | 反例（禁止） |
|---|------|-------------|
| 1 | **强类型图像容器**：`Image<T, const C: usize>` 泛型，`T: Pixel`（u8/u16/f32/…），`C` 为编译期常量通道数 | 禁止 `CV_8UC1` 魔术数字运行时判断 |
| 2 | **零拷贝视图**：ROI 是 `ImageView<'a, T, C>`（借用），非引用计数智能指针 | 禁止 `cv::Mat` 式的隐式拷贝/共享 |
| 3 | **opencv-python 返回风格**：公开函数一律返回新分配的 `Image`（如 `gaussian_blur(&src, ...) -> Image<T, C>`），不接受输出参数。确有零分配需求的实时管线场景，可提供显式 `*_in_place(&mut img)` 方法变体（仅实测分配热点时落地） | 禁止 C++ 式 `&mut dst` 输出参数、`OutputArray` 模糊语义 |
| 4 | **边界策略显式枚举**：`enum BorderPolicy<T: Pixel> { Clamp, Reflect, Constant(T) }`，与 `Image<T, C>` 的 `T` 绑定。`Constant` 值类型必须与图像像素类型一致，禁止隐式转换 | 禁止 OpenCV 式的隐式 `BORDER_DEFAULT` |
| 5 | **错误分类即类型**：IO/编解码等**真实可失败**操作返回 `Result<T, Error>`（错误包含路径/格式信息）；纯计算函数直接返回 `Image`——输入合法性由 `Image<T, C>` 类型系统保证（维度/通道/类型错误在编译期排除），残余非法参数（如偶数核大小）属程序员错误，用 `panic!` + `# Panics` 文档标注（对齐 std 惯例：`Vec` 越界即 panic） | 禁止返回 `-1` 或空 `Mat`；禁止纯计算函数包一层无意义的 `Result` |
| 6 | **公开 API 一律 RGB**：彩色图像三通道顺序为 R、G、B。与 OpenCV 对拍时仅在 `compare/` 层翻转通道一次 | 禁止在库内做 BGR 兼容分支 |

### 公开 API 签名规范（铁律 3 + 5 的落地形式）

```rust
// ✅ 纯计算函数：opencv-python 风格，返回新分配的 Image
pub fn gaussian_blur<T: Pixel, const C: usize>(
    src: &Image<T, C>,
    ksize: (usize, usize),
    sigma: f64,
    border: BorderPolicy<T>,
) -> Image<T, C>;

// ✅ 通道数变化的函数：返回类型显式标注
pub fn to_grayscale<T: Pixel>(src: &Image<T, 3>) -> Image<T, 1>;

// ✅ IO 函数：真实可失败，返回 Result
pub fn imread<P: AsRef<Path>>(path: P) -> Result<Image<u8, 3>>;
pub fn imwrite<P: AsRef<Path>, const C: usize>(path: P, img: &Image<u8, C>) -> Result<()>;

// ❌ 禁止：C++ 式输出参数
pub fn gaussian_blur<T: Pixel, const C: usize>(src: &Image<T, C>, dst: &mut Image<T, C>, ...);

// ❌ 禁止：纯计算包 Result（输入合法性已由类型系统保证）
pub fn threshold<T: Pixel, const C: usize>(...) -> Result<Image<T, C>, Error>;

// ⚠️ 例外：仅当实测分配成为热点时，可补充显式 in-place 变体（命名必须带 _in_place）
pub fn gaussian_blur_in_place<T: Pixel, const C: usize>(img: &mut Image<T, C>, ...);
```

- 非法参数（偶数核、空图、`ksize.0 == 0` 等）→ `panic!`，公开函数文档必须含 `# Panics` 段
- 泛型参数顺序固定：`<T: Pixel, const C: usize>`（通道数变化的函数按需调整）

### Pixel trait 契约

```rust
pub trait Pixel: Copy + Send + Sync + 'static + PartialOrd {
    /// 用于 SIMD 加载/存储的类型，通常与 Self 相同
    type Simd: Copy;
    /// 加法饱和/回绕语义由实现决定，文档必须写明
    fn zero() -> Self;
    fn max_value() -> Self;
    /// 转 f32 用于需要浮点中间结果的算法（如高斯）
    fn to_f32(self) -> f32;
    fn from_f32(v: f32) -> Self;
}

impl Pixel for u8 { ... }
impl Pixel for u16 { ... }
impl Pixel for f32 { ... }
```

- 禁止为 `bool`、`i32`、自定义类型实现 `Pixel`，除非有明确算法需求
- `to_f32`/`from_f32` 的舍入策略必须在 trait 文档写明（默认 round-to-nearest）

### Error 契约（仅 IO/编解码层）

`Error` 只覆盖**运行期真实可失败**的场景（文件不存在、解码失败、编码格式不支持）。纯计算函数不返回 `Result`（铁律 5）。

```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("codec: {0}")]
    Codec(String),
    #[error("unsupported pixel type: {0}")]
    UnsupportedPixelType(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
```

- 禁止 `Error::Other(String)` 兜底；新增错误变体需在 PR 说明触发场景
- 错误信息必须包含可定位的数值（路径、格式名），便于工业现场调试
- 纯计算函数的非法参数（偶数核、空图、越界 ROI）→ `panic!`，并在 `# Panics` 文档段写明触发条件

---

## 2. 性能目标

**唯一对标基线：OpenCV 5.0（`/opt/opencv5`），同机同图同参数。**

**强制实现原则：每个算法直接参考 OpenCV5 源码（`/workspace/programs/c++/opencv-5.0.0/modules/…`）的 SIMD 内核，一步到位实现 AVX2 + AVX512 + rayon 并行化。禁止先实现标量版再回头优化——那只会浪费算力，结果与 OpenCV5 差一大截，没有意义。**

| 函数类别 | 吞吐量目标 | 说明 |
|----------|-----------|------|
| 计算密集型（滤波、梯度、颜色转换、阈值、边缘检测、形态学） | **≥ 1.0×**，力争 **≥ 1.2×** | 主要向量化收益战场 |
| 归约型（直方图、矩、范数） | **≥ 0.9×** | 瓶颈常在标量归约，允许持平 |
| IO/编解码（imgcodecs） | **≥ 1.0×** | 对标 image crate 与 OpenCV5 |

**正确性底线：与 OpenCV5 同参数输出逐像素对比（彩色图先做 RGB↔BGR 翻转），PSNR > 60dB。**

### SIMD 策略（强制：直接参考 OpenCV5 源码，禁止先实现标量版）

**核心原则：每个算法落地时，直接参考 OpenCV5 对应源码（`/workspace/programs/c++/opencv-5.0.0/modules/…`）的 SIMD 内核，一步到位实现 AVX2/AVX512 内核 + rayon 并行化。禁止先实现一版标量再回头优化——那只会浪费算力，且结果与 OpenCV5 差一大截，没有意义。**

| 层级 | 指令集 | 角色 | 启用条件 |
|------|--------|------|---------|
| 一级优化 | AVX2 | 手写内核，主力路径 | 默认启用，覆盖当前绝大多数 x86 部署环境 |
| 二级优化 | AVX512 | 手写内核，可选加速 | 与 AVX2 内核**同时落地**，运行时派发；仅当 bench 证明相对 AVX2 有 **≥ 1.3× 稳定优势**时作为默认路径 |
| 兜底 | 标量迭代器 | 自动向量化，正确性基准 | 所有平台始终保留（仅作 fallback，非主路径） |

- **实现顺序**：先精读 OpenCV5 对应源码的 SIMD 内核（`*.simd.hpp` / `*.dispatch.cpp`），理解其向量数据流、舍入方式、边界处理，再在 Rust 中复刻。禁止凭空写标量版再优化。
- 运行时派发：`AVX512 → AVX2 → 标量`，使用 `is_x86_feature_detected!` 一次探测并缓存（`OnceLock`）
- ARM 路径：`NEON → 标量`，按需落地，不预建
- **AVX512 与 AVX2 内核同时实现**，不因"无 bench 数据"而推迟 AVX512；bench 数据用于决定默认路径，而非决定是否实现
- 每个 SIMD 内核必须附 `// SAFETY:` 注释说明前置条件（对齐、长度、边界）

### 并行策略（强制：rayon 行并行，与 SIMD 内核同时落地）

- 每个算法落地时**同时**实现 rayon 行并行（`par_chunks_exact_mut`），与 SIMD 内核一起交付，禁止先单线程后补并行
- 触发条件：图像 > 1MP 且操作计算密集（滤波、梯度、颜色转换、阈值、形态学、边缘检测）
- 行并行粒度：每行一个任务，`par_chunks_exact_mut` 按行切分，避免跨行依赖
- 参考 OpenCV5 的 `parallel_for_` 行分块策略，对齐其并行度
- BufferPool 预分配：仅当出现实时管线、分配成为实测热点时引入

### 编译调优

- release：`lto = "fat"`, `codegen-units = 1`

---

## 3. Rust 代码风格约束

| # | 约束 | 说明 |
|---|------|------|
| 1 | **unsafe 隔离**：crate 级 `#![warn(unsafe_code)]`；CI 脚本 `scripts/check_unsafe.sh` 检查 `simd_kernels/` 之外无 `unsafe` 块，发现即失败。`simd_kernels/` 内每处 `unsafe` 必须附 `// SAFETY:` 注释说明前置条件 | 用 warn + CI 检查替代 deny，因 crate 级 deny 无法被子模块 allow 覆盖 |
| 2 | **拒绝 C++ 式 Rust**：不用 `Rc<RefCell<_>>` 模拟 `Mat` 共享，不用 `Box<dyn Trait>` 做类型擦除 | 用泛型、用枚举、用视图 |
| 3 | **迭代器优先**：像素级操作用 `chunks_exact`/`windows`，让编译器自动向量化 | 手写索引循环需 bench 证明更快 |
| 4 | **常量传播**：核大小、通道数尽量用 `const N: usize`，编译期展开小核卷积 | `fn convolve3x3<const C: usize>(...)` |
| 5 | **文档即契约**：公开函数写清复杂度；性能数据只写实测值 | 禁止未测量就宣称 "Nx faster than OpenCV" |
| 6 | **SIMD 实现四层结构**：公开入口做派发，`#[target_feature]` AVX2 内核，`#[target_feature]` AVX512 内核，标量版本作为 fallback 与测试基准 | 禁止把 SIMD 逻辑写进入口函数 |
| 7 | **命名对齐 OpenCV，风格对齐 Rust**：模块名用 OpenCV 的（`imgproc`、`imgcodecs`），函数名用 OpenCV 的 snake_case 直译（`cvt_color` → `to_grayscale`、`threshold`、`sobel`、`canny`）。同一概念禁止多个别名。公开类型用 `PascalCase`，禁止 `cv` 前缀 | 禁止 `cv_to_gray`、`ToGray`、`toGray` 混用 |
| 8 | **通道顺序 RGB**：公开 API 的彩色图一律 RGB。函数名、文档、示例中不得出现 BGR 字样，唯一例外是 `compare/` 层的显式翻转说明 | 禁止 `to_bgr`、`bgr_to_gray` 等命名 |
| 9 | **返回值风格（opencv-python 式）**：公开函数签名形如 `fn xxx<T: Pixel, const C: usize>(src: &Image<T, C>, ...) -> Image<T, C>`——输入只读借用，输出全新分配，无输出参数。调用方式与 `cv2.GaussianBlur(src, ksize)` 返回新 Mat 一致。IO 函数（`imread`/`imwrite`）返回 `Result` | 禁止 `fn xxx(src: &Image<T,C>, dst: &mut Image<T,C>)`；禁止 `fn xxx(...) -> Result<Image>` 包裹纯计算 |

---

## 4. 验证标准

每个算法合并前必须通过：

| 检查项 | 标准 |
|--------|------|
| **对照正确性** | 与 OpenCV5 同参数输出逐像素对比，PSNR > 60dB |
| **对照速度** | criterion benchmark：记录实测 MPix/s，与 OpenCV5 同机同图对比数据一并提交 |
| **单元测试** | 手工可算的小矩阵输入/输出；边界尺寸（0 像素、单行）有覆盖 |
| **SIMD 双路径交叉验证** | 每个 SIMD 内核必须有"标量 vs AVX2 vs AVX512"三路径交叉验证单测（`isa_cross_tests` 模式：同一输入分别调三个实现，断言逐位一致；CPU 不支持某 ISA 时该分支 skip）。防止迁移到仅支持 AVX2 的机器时暴露未测路径缺陷 |

**SIMD 双路径工程规则**（AVX2 与 AVX512 同等对待，缺一不可）：

1. **AVX2 内核禁止使用 AVX512VL 专属 intrinsic**（如 `_mm256_cvtusepi32_epi16`、`_mm256_cvtusepi16_epi8`）。`lto = "fat"` 下，`#[target_feature(enable = "avx2")]` 函数内的 AVX512VL intrinsic 会被 LLVM outline 成带运行时检测的函数调用（每条向量指令一次 call），性能崩塌 10 倍以上且难以从 bench 发现。等价替代：`_mm256_packus_epi32` + `_mm256_permute4x64_epi64(p, 0xD8)` lane 修复 + SSE `_mm_packus_epi16`。
2. **`#[target_feature]` 内核必须显式 `#[inline(never)]`**（或经 `#[inline(never)]` 派发入口调用）。LTO 会把 target_feature 内核内联进普通函数（如 imgproc 层的流式路径），intrinsic 失去特性上下文后同样被 outline 成函数调用。提交前用 `objdump -d <bin> | grep "call.*core_arch"` 检查：**rcv 自身代码内不允许出现 core_arch 包装函数调用**（std 内部 panic 路径除外）。
3. **回退/重构 SIMD 代码时必须核对 `#[target_feature]` 属性完整**——属性丢失后函数退化为普通 unsafe fn，intrinsic 全部 outline，编译不报错、测试不失败（结果仍正确），只有性能崩塌 10-20 倍。这是静默性能回退，唯一防线是 objdump 检查 + bench 基线对比。

**明确不需要**：
- 分阶段 bench 流程
- 历史回归检查
- 内存剖析（除非实测出现分配热点）
- 泛型维度抽象（不用 ndarray）
- `Box<dyn Trait>` 类型擦除
- `Rc<RefCell<_>>` 共享
- feature flag 控制算法可用性——算法要么落地要么不存在
- 为未实现的 OpenCV 函数预建模块或空函数

### 4.5 OpenCV5 对拍机制

- OpenCV5 安装于 `/opt/opencv`。CMake 引用路径：
  - `-DOpenCV_DIR=/opt/opencv/lib/cmake/opencv5`
  - 头文件 `/opt/opencv/include/opencv5/`
  - 库 `/opt/opencv/lib/`
- `compare/` 目录下维护 `compare_opencv5.cpp`，编译为 `opencv5_ref` 可执行文件
- **彩色图对拍时，`compare_opencv5.cpp` 负责 RGB↔BGR 翻转**：Rust 侧读入 RGB 图 → 传给 ref 前翻为 BGR → OpenCV5 处理 → 输出前翻回 RGB → dump bin。Rust 侧不做翻转
- 输入：`compare/inputs/` 下的测试图（至少含：1×1、单行、单列、非方阵、灰度、RGB 彩色、u8、f32）
- 输出：`compare/ref/<函数名>_<参数哈希>.bin`，原始像素 dump
- `scripts/compare.sh <函数名>` 是唯一入口：编译 ref → 跑 ref → `cargo run --example compare -- <函数名>` → 算 PSNR → 输出报告
- `examples/compare.rs` 只负责：读 ref bin、读本库输出、逐像素比较、打印结果，不做编排放
- PSNR 计算用固定公式，参考实现放在 `scripts/psnr.py`
- **没有对应 ref bin 的函数，视为未验证，不允许合并**
- 禁止把 OpenCV 调用逻辑写进 Rust 侧

---

## 5. 技术栈

**当前依赖**（最小集，随阶段按需增加）：

```toml
[dependencies]
image = { version = "0.25", default-features = false, features = ["png", "jpeg"] }  # 仅 imgcodecs 用作编解码层
rayon = "1"        # 行并行
thiserror = "1"    # Error 定义

[dev-dependencies]
criterion = "0.5"  # 严格基准
```

**预留依赖**（进入对应模块时再加）：

| 库 | 引入时机 |
|----|---------|
| nalgebra | 标定模块（calib3d）落地时 |
| dhat | 实测出现分配热点时（dev-dependency） |

> 不需要 ndarray：`Image<T, C>` 的连续 `Vec<T>` 就是最优布局，ndarray 的通用维度抽象对本库是负担。

---

## 6. 代码架构

模块命名与 OpenCV 严格对齐（方便对照源码），内部设计遵循第 1 节铁律（不照搬 Mat）。
**按需创建：下列目录只有落地对应功能时才新建，不预建空壳。**

```
src/
├── core/           # 对应 modules/core
│   ├── image.rs    # Image<T, const C: usize>、Pixel（对应 mat.hpp，但强类型）
│   ├── border.rs   # BorderPolicy（铁律 4，对应 borderInterpolate）
│   ├── error.rs    # Error、Result（铁律 5，仅 IO/编解码层使用）
│   ├── point.rs    # Point（对应 cv::Point）
│   ├── rect.rs     # Rect、Size（对应 cv::Rect、cv::Size）
│   └── arithm.rs   # 逐像素算术：invert（对应 arithm.cpp 的 bitwise_not）
├── imgcodecs/      # 对应 modules/imgcodecs：imread / imwrite
├── imgproc/        # 对应 modules/imgproc
│   ├── color.rs        # to_grayscale / to_rgb（对应 color.simd.hpp 的 cvtColor）
│   ├── threshold.rs    # threshold / adaptive_threshold（对应 thresh.cpp）
│   ├── filter.rs       # box_blur / gaussian_blur / median_blur / bilateral_filter
│   ├── morphology.rs   # erode / dilate / morph_open / morph_close / morph_gradient
│   ├── gradient.rs     # sobel / scharr / laplacian / magnitude / phase / cart_to_polar
│   ├── edges.rs        # canny
│   ├── transform.rs    # warp_affine / warp_perspective / resize（仿射/透视/缩放）
│   ├── distance.rs     # distance_transform（距离变换）
│   ├── contours.rs     # find_contours / contour_area / bounding_rect / approx_poly_dp
│   └── histogram.rs    # calc_hist / equalize_hist / compare_hist
├── simd_kernels/   # 手写 SIMD 内循环（唯一允许 unsafe 的模块）
│   ├── x86/        # AVX2（主力）、AVX512（同时落地，运行时派发）
│   ├── arm/        # NEON（按需）
│   └── scalar.rs   # 标量 fallback，所有平台的正确性基准
└── lib.rs          # 公开 API 重导出，内部模块路径不对外承诺稳定

# 用到时再建：
# features2d/       # 特征点、描述子（对齐 OpenCV features2d）
# calib3d/          # 相机标定、手眼标定（对齐 OpenCV calib3d）
compare/
├── compare_opencv5.cpp   # OpenCV5 参考实现（含 RGB↔BGR 翻转）
├── inputs/               # 测试图
└── ref/                  # ref bin 输出
scripts/
├── compare.sh            # 对拍唯一入口
├── psnr.py               # PSNR 计算
└── check_unsafe.sh       # unsafe 隔离检查
examples/
├── bench.rs              # 轻量吞吐基准（Instant 计时）
└── compare.rs            # 读 ref bin、逐像素对比
```

---

## 6.5 落地顺序（强制）

按下列顺序逐个函数落地，**完成一个再开始下一个，禁止并行铺开**。

| 阶段 | 函数 | 模块 | 备注 |
|------|------|------|------|
| 0 | `Image`、`Pixel`、`Error`、`BorderPolicy`、`Point`、`Rect`、`Size` | `core/` | 基础设施 |
| 1 | `imread`、`imwrite` | `imgcodecs/` | PNG 优先，JPEG 次之 |
| 2 | `to_grayscale`、`to_rgb` | `imgproc/color.rs` | 公开 API 输入/输出均 RGB |
| 3 | `threshold`、`adaptive_threshold` | `imgproc/threshold.rs` | |
| 4 | `box_blur`、`gaussian_blur`、`median_blur`、`bilateral_filter` | `imgproc/filter.rs` | |
| 5 | `erode`、`dilate`、`morph_open`、`morph_close`、`morph_gradient` | `imgproc/morphology.rs` | |
| 6 | `sobel`、`scharr`、`laplacian`、`magnitude`、`phase`、`cart_to_polar` | `imgproc/gradient.rs` | |
| 7 | `canny` | `imgproc/edges.rs` | |
| 8 | `resize`、`warp_affine`、`warp_perspective` | `imgproc/transform.rs` | 插值策略显式枚举 |
| 9 | `distance_transform` | `imgproc/distance.rs` | |
| 10 | `calc_hist`、`equalize_hist`、`compare_hist` | `imgproc/histogram.rs` | |
| 11 | `find_contours`、`contour_area`、`bounding_rect`、`approx_poly_dp` | `imgproc/contours.rs` | |

**每个函数完成标准**：
1. 单元测试通过（手工可算的小矩阵 + 边界尺寸）
2. 与 OpenCV5 对比 PSNR > 60dB
3. **AVX2 内核 + AVX512 内核 + rayon 行并行同时落地**（参考 OpenCV5 源码，禁止先标量版）
4. bench 数据提交（MPix/s，与 OpenCV5 同机同图对比，单线程 + 多线程），目标 ≥ 1.0×，力争 ≥ 1.2×
5. `scripts/check_unsafe.sh` 通过

---

## 7. Agent 角色指令

你是该项目的 Rust 视觉系统架构师。你的职责不是把 C++ 翻译成 Rust，而是用 Rust 的类型系统和零成本抽象重新设计算法。每段生成的代码必须满足：

**架构上**
- 符合第 1 节铁律，模块归属按第 6 节对号入座
- 不预建空模块，不提前引入预留依赖
- 公开 API 一律 RGB，禁止 BGR 兼容分支

**性能上**
- **直接参考 OpenCV5 源码实现**：每个算法落地前，先精读 `/workspace/programs/c++/opencv-5.0.0/modules/…` 对应源码的 SIMD 内核（`*.simd.hpp` / `*.dispatch.cpp`），理解其向量数据流、舍入、边界处理，再在 Rust 中复刻。**禁止先实现一版标量再回头优化**——那只会浪费算力，结果与 OpenCV5 差一大截，没有意义。
- **一步到位**：AVX2 内核 + AVX512 内核 + rayon 行并行**同时落地**，一次交付，禁止分阶段（先标量、再 AVX2、再并行）。
- 运行时派发 `AVX512 → AVX2 → 标量`，用 `is_x86_feature_detected!` 探测并缓存（`OnceLock`）
- 所有性能结论必须来自与 OpenCV5（`/opt/opencv`）同机同图的实测对比，目标 ≥ 1.0×，力争 ≥ 1.2×

**风格上**
- 拒绝 unsafe 和 `Rc<RefCell>`
- 优先泛型单态化
- SIMD 实现严格四层结构：派发入口 / `#[target_feature]` AVX2 内核 / `#[target_feature]` AVX512 内核 / 标量 fallback
- 命名对齐 OpenCV snake_case，禁止别名

**验证上**
- 每个算法必须通过与 OpenCV5 的逐像素对比（PSNR > 60dB）
- 附带单元测试（手工可算的小矩阵）
- 提交时一并附上实测吞吐量（MPix/s）与 OpenCV5 对比数据

**响应粒度**
- 单次响应只实现一个函数，附带其单元测试和对比脚本调用
- 禁止一次性生成多个函数
- 禁止生成未在 §6.5 落地顺序中的函数
- 如遇阻塞（如缺 ref bin、编译失败），先报告阻塞点，不自行绕过

**禁止事项**
- **禁止先实现标量版再回头优化**——每个算法必须直接参考 OpenCV5 源码，一步到位实现 AVX2 + AVX512 + rayon 并行化
- **禁止分阶段交付**（先标量、再 AVX2、再 rayon）——AVX2/AVX512/rayon 必须一次交付
- 禁止在无 bench 数据的情况下宣称性能优势
- 禁止为追求 AVX512 而牺牲 AVX2 路径的覆盖
- 禁止把 SIMD 内联进业务逻辑，必须隔离在 `simd_kernels/`
- 禁止 `Error::Other(String)` 兜底
- 禁止在库内出现 `bgr` 字样（`compare/` 层除外）

---

## 附录 A：对拍骨架代码

### A.1 `compare/compare_opencv5.cpp`

```cpp
// OpenCV5 参考实现，编译为 opencv5_ref。
// 用法: opencv5_ref <func> <input.png> <output.bin> [args...]
//
// 约定：
//   - Rust 侧输入/输出一律 RGB。
//   - 本程序负责 RGB<->BGR 翻转，OpenCV5 内部仍用 BGR。
//   - 输出为原始像素 dump（无头），Rust 侧按同维度读取。
//
// 编译（OpenCV5 安装于 /opt/opencv）：
//   g++ -std=c++17 -O3 compare_opencv5.cpp -o opencv5_ref \
//       -I/opt/opencv/include/opencv5 \
//       -L/opt/opencv/lib -lopencv_core -lopencv_imgproc -lopencv_imgcodecs
//
// 运行前：
//   export LD_LIBRARY_PATH=/opt/opencv/lib:$LD_LIBRARY_PATH

#include <opencv2/opencv.hpp>
#include <opencv2/imgproc.hpp>
#include <opencv2/imgcodecs.hpp>
#include <iostream>
#include <string>
#include <vector>
#include <cstring>

using namespace cv;

// ---------- 工具 ----------

static void dump_bin(const std::string& path, const Mat& m) {
    CV_Assert(m.isContinuous());
    std::ofstream ofs(path, std::ios::binary);
    ofs.write(reinterpret_cast<const char*>(m.data),
              static_cast<std::streamsize>(m.total() * m.elemSize()));
}

// Rust 侧传 RGB，OpenCV 用 BGR，这里做一次翻转（3 通道时）
static Mat rgb_to_bgr_in(const Mat& rgb) {
    Mat bgr;
    if (rgb.channels() == 3) cvtColor(rgb, bgr, COLOR_RGB2BGR);
    else bgr = rgb.clone();
    return bgr;
}

// OpenCV 输出 BGR，翻回 RGB 交给 Rust
static Mat bgr_to_rgb_out(const Mat& bgr) {
    Mat rgb;
    if (bgr.channels() == 3) cvtColor(bgr, rgb, COLOR_BGR2RGB);
    else rgb = bgr.clone();
    return rgb;
}

// ---------- 各函数参考实现 ----------
// 每个分支：
//   1. 读取输入（RGB）
//   2. 翻为 BGR 喂给 OpenCV5
//   3. 调用对应 OpenCV5 API（参数必须与 Rust 侧完全一致）
//   4. 输出翻回 RGB
//   5. dump 原始像素

static int run_to_grayscale(const std::string& in, const std::string& out) {
    Mat rgb = imread(in, IMREAD_COLOR);
    if (rgb.empty()) { std::cerr << "imread failed\n"; return 1; }
    Mat bgr = rgb_to_bgr_in(rgb);
    Mat gray;
    cvtColor(bgr, gray, COLOR_BGR2GRAY);
    dump_bin(out, gray);
    return 0;
}

static int run_threshold(const std::string& in, const std::string& out,
                         double thresh, double maxval) {
    Mat rgb = imread(in, IMREAD_GRAYSCALE);
    if (rgb.empty()) { std::cerr << "imread failed\n"; return 1; }
    Mat dst;
    threshold(rgb, dst, thresh, maxval, THRESH_BINARY);
    dump_bin(out, dst);
    return 0;
}

static int run_gaussian_blur(const std::string& in, const std::string& out,
                             int ksize, double sigma) {
    Mat rgb = imread(in, IMREAD_COLOR);
    if (rgb.empty()) { std::cerr << "imread failed\n"; return 1; }
    Mat bgr = rgb_to_bgr_in(rgb);
    Mat dst;
    GaussianBlur(bgr, dst, Size(ksize, ksize), sigma);
    dump_bin(out, bgr_to_rgb_out(dst));
    return 0;
}

static int run_erode(const std::string& in, const std::string& out, int ksize) {
    Mat gray = imread(in, IMREAD_GRAYSCALE);
    if (gray.empty()) { std::cerr << "imread failed\n"; return 1; }
    Mat kernel = getStructuringElement(MORPH_RECT, Size(ksize, ksize));
    Mat dst;
    erode(gray, dst, kernel);
    dump_bin(out, dst);
    return 0;
}

static int run_dilate(const std::string& in, const std::string& out, int ksize) {
    Mat gray = imread(in, IMREAD_GRAYSCALE);
    if (gray.empty()) { std::cerr << "imread failed\n"; return 1; }
    Mat kernel = getStructuringElement(MORPH_RECT, Size(ksize, ksize));
    Mat dst;
    dilate(gray, dst, kernel);
    dump_bin(out, dst);
    return 0;
}

static int run_canny(const std::string& in, const std::string& out,
                     double low, double high) {
    Mat gray = imread(in, IMREAD_GRAYSCALE);
    if (gray.empty()) { std::cerr << "imread failed\n"; return 1; }
    Mat dst;
    Canny(gray, dst, low, high);
    dump_bin(out, dst);
    return 0;
}

// ---------- 入口 ----------

int main(int argc, char** argv) {
    if (argc < 4) {
        std::cerr << "usage: opencv5_ref <func> <input> <output.bin> [args...]\n";
        return 2;
    }
    std::string func = argv[1];
    std::string in   = argv[2];
    std::string out  = argv[3];

    if (func == "to_grayscale")    return run_to_grayscale(in, out);
    if (func == "threshold")       return run_threshold(in, out, std::stod(argv[4]), std::stod(argv[5]));
    if (func == "gaussian_blur")   return run_gaussian_blur(in, out, std::stoi(argv[4]), std::stod(argv[5]));
    if (func == "erode")           return run_erode(in, out, std::stoi(argv[4]));
    if (func == "dilate")          return run_dilate(in, out, std::stoi(argv[4]));
    if (func == "canny")           return run_canny(in, out, std::stod(argv[4]), std::stod(argv[5]));

    std::cerr << "unknown func: " << func << "\n";
    return 3;
}
```

### A.2 `scripts/compare.sh`

```bash
#!/usr/bin/env bash
# 唯一对拍入口：编译 ref -> 跑 ref -> 跑 rust -> 算 PSNR -> 输出报告。
#
# 用法:
#   scripts/compare.sh <func> <input.png> [args...]
#
# 例:
#   scripts/compare.sh to_grayscale compare/inputs/lena.png
#   scripts/compare.sh gaussian_blur compare/inputs/lena.png 5 1.4
#
# 依赖:
#   - OpenCV5 安装于 /opt/opencv
#   - g++ 支持 C++17
#   - python3（用于 psnr.py）

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OPENCV_DIR="${OPENCV_DIR:-/opt/opencv}"
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
        -lopencv_core -lopencv_imgproc -lopencv_imgcodecs
fi

# 2. 跑 ref
echo "[compare] running ref: ${FUNC} ${INPUT} $*"
LD_LIBRARY_PATH="${OPENCV_DIR}/lib:${LD_LIBRARY_PATH:-}" \
    "${REF_BIN}" "${FUNC}" "${INPUT}" "${REF_OUT}" "$@"

# 3. 跑 rust
echo "[compare] running rust: ${FUNC} ${INPUT} $*"
cargo run --release --example compare -- \
    "${FUNC}" "${INPUT}" "${RUST_OUT}" "$@"

# 4. 算 PSNR
echo "[compare] computing PSNR ..."
python3 "${ROOT}/scripts/psnr.py" "${REF_OUT}" "${RUST_OUT}"

echo "[compare] done. ref=${REF_OUT} rust=${RUST_OUT}"
```

### A.3 `scripts/psnr.py`

```python
#!/usr/bin/env python3
"""逐像素 PSNR 计算。输入为两个原始像素 dump（无头），要求长度一致、类型一致。"""

import sys
import numpy as np

# 阈值：PSNR > 60dB 视为通过（与纲领 §4 一致）
PASS_THRESHOLD = 60.0


def psnr(a: np.ndarray, b: np.ndarray, max_val: float) -> float:
    if a.shape != b.shape:
        raise ValueError(f"shape mismatch: {a.shape} vs {b.shape}")
    if a.size == 0:
        return float("inf")
    diff = a.astype(np.float64) - b.astype(np.float64)
    mse = float(np.mean(diff * diff))
    if mse == 0.0:
        return float("inf")
    return 10.0 * np.log10((max_val * max_val) / mse)


def infer_max_val(path: str, data: np.ndarray) -> float:
    # 简单启发式：按文件名后缀或 dtype 推断
    # 目前约定 dump 为 u8 或 f32 两种，其他由调用方扩展
    if data.dtype == np.uint8:
        return 255.0
    if data.dtype == np.float32:
        return 1.0
    # 未知类型，按数据最大值估计
    return float(data.max()) if data.size else 1.0


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <ref.bin> <rust.bin>", file=sys.stderr)
        return 2

    ref_path, rust_path = sys.argv[1], sys.argv[2]

    # 先按 u8 读，长度不一致再按 f32 读；实际项目可加 --dtype 参数
    ref_bytes = np.fromfile(ref_path, dtype=np.uint8)
    rust_bytes = np.fromfile(rust_path, dtype=np.uint8)

    if ref_bytes.size != rust_bytes.size:
        print(f"size mismatch: ref={ref_bytes.size} rust={rust_bytes.size}",
              file=sys.stderr)
        return 3

    max_val = infer_max_val(ref_path, ref_bytes)
    value = psnr(ref_bytes, rust_bytes, max_val)

    status = "PASS" if value >= PASS_THRESHOLD else "FAIL"
    print(f"PSNR = {value:.3f} dB  (threshold {PASS_THRESHOLD} dB)  [{status}]")

    return 0 if status == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
```

### A.4 `scripts/check_unsafe.sh`

```bash
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
```

---

## 附录 B：使用说明（给 Agent 与人类协作者）

1. **首次落地**：按 §6.5 阶段 0 开始，逐阶段推进。每完成一个函数，运行 `scripts/compare.sh <func> <input>` 验证 PSNR。
2. **对拍前置**：确认 `/opt/opencv5` 下存在 `include/opencv5/`、`lib/libopencv_world.so` 等。若路径不同，通过 `OPENCV_DIR` 环境变量覆盖。
3. **新增函数**：先在 `compare/compare_opencv5.cpp` 增加对应分支，再实现 Rust 侧，最后跑 `scripts/compare.sh`。
4. **SIMD 引入（强制）**：每个算法落地时，直接参考 OpenCV5 源码（`/workspace/programs/c++/opencv-5.0.0/modules/…`）的 SIMD 内核，在 `simd_kernels/x86/` 同时实现 AVX2 + AVX512 内核，并配 rayon 行并行。禁止先实现标量版再优化。
5. **AVX512**：与 AVX2 同时落地，运行时派发；bench 数据用于决定默认路径（AVX512 需相对 AVX2 ≥ 1.3× 才作默认），而非决定是否实现。
6. **双路径测试（强制）**：每个 SIMD 内核配"标量 vs AVX2 vs AVX512"三路径交叉验证单测（§4）；AVX2 内核禁用 AVX512VL intrinsic；`#[target_feature]` 内核显式 `#[inline(never)]`（§4 SIMD 双路径工程规则）。
7. **提交前检查**：`scripts/check_unsafe.sh` 必须通过；PSNR 必须 > 60dB；bench 数据（单线程 + 多线程）必须与 OpenCV5 一并提交，目标 ≥ 1.0×，力争 ≥ 1.2×；`objdump -d target/release/examples/bench | grep "call.*core_arch"` 中不允许出现 rcv 代码的 outline 调用（静默性能回退防线）。
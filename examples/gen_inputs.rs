//! 生成对拍测试图到 compare/inputs/。
//!
//! 用法: cargo run --release --example gen_inputs
//!
//! 生成：1×1、单行、单列、非方阵、灰度、RGB 彩色（u8）。

use rcv::{Image, imwrite};

fn main() {
    let dir = std::path::Path::new("compare/inputs");
    std::fs::create_dir_all(dir).expect("create inputs dir");

    // 1×1 RGB
    imwrite(
        dir.join("one_px.png"),
        &Image::<u8, 3>::from_pixel(1, 1, [200, 100, 50]),
    )
    .unwrap();

    // 单行 RGB
    imwrite(
        dir.join("single_row.png"),
        &Image::<u8, 3>::from_fn(16, 1, |x, _| [x as u8, (255 - x) as u8, (x * 3) as u8]),
    )
    .unwrap();

    // 单列 RGB
    imwrite(
        dir.join("single_col.png"),
        &Image::<u8, 3>::from_fn(1, 16, |_, y| [(y * 4) as u8, (y * 2) as u8, y as u8]),
    )
    .unwrap();

    // 非方阵 RGB
    imwrite(
        dir.join("non_square.png"),
        &Image::<u8, 3>::from_fn(24, 7, |x, y| {
            [(x * 10) as u8, (y * 30) as u8, ((x + y) * 5) as u8]
        }),
    )
    .unwrap();

    // 灰度（非方阵）
    imwrite(
        dir.join("gray.png"),
        &Image::<u8, 1>::from_fn(20, 13, |x, y| [((x * 7 + y * 3) % 256) as u8]),
    )
    .unwrap();

    // 彩色渐变（较大，用于性能/精度）
    imwrite(
        dir.join("gradient.png"),
        &Image::<u8, 3>::from_fn(64, 48, |x, y| [x as u8, y as u8, ((x + y) / 2) as u8]),
    )
    .unwrap();

    // 大图（1024×1024，用于性能基准）
    imwrite(
        dir.join("bench.png"),
        &Image::<u8, 3>::from_fn(1024, 1024, |x, y| {
            [
                ((x * 7 + y * 3) % 256) as u8,
                ((x * 5 + y * 11) % 256) as u8,
                ((x + y) % 256) as u8,
            ]
        }),
    )
    .unwrap();

    println!("inputs generated in compare/inputs/");
}

use crate::core::image::Dims;

/// 库统一错误类型（铁律 5：错误即类型）。
///
/// 所有公开算法返回 [`Result`]，错误信息携带可定位的数值（维度、类型、范围），
/// 便于工业现场调试。禁止 `Other(String)` 式兜底；新增变体需在 PR 说明触发场景。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 维度不匹配（如 ROI 越界、缓冲长度与声明维度不符）。
    #[error("dimension mismatch: expected {expected:?}, got {actual:?}")]
    DimensionMismatch { expected: Dims, actual: Dims },

    /// 不支持的通道数（如算法仅支持 1/3 通道时收到其他值）。
    #[error("unsupported channel count: {0}")]
    UnsupportedChannels(usize),

    /// 不支持的像素类型。
    #[error("unsupported pixel type: {0}")]
    UnsupportedPixelType(&'static str),

    /// 数值越界（如参数超出合法区间）。
    #[error("value out of range: {value} not in [{min}, {max}]")]
    OutOfRange { value: f64, min: f64, max: f64 },

    /// 空输入（算法要求非空图像/点集）。
    #[error("empty input")]
    EmptyInput,

    /// IO 错误（imgcodecs 读写文件）。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// 编解码错误（imgcodecs 层）。
    #[error("codec: {0}")]
    Codec(String),
}

/// 库统一结果类型。
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_mismatch_display_contains_values() {
        let e = Error::DimensionMismatch {
            expected: Dims {
                width: 640,
                height: 480,
                channels: 3,
            },
            actual: Dims {
                width: 320,
                height: 240,
                channels: 3,
            },
        };
        let s = e.to_string();
        assert!(s.contains("640"));
        assert!(s.contains("320"));
        assert!(s.contains("480"));
        assert!(s.contains("240"));
    }

    #[test]
    fn scalar_variants_display() {
        assert_eq!(
            Error::UnsupportedChannels(5).to_string(),
            "unsupported channel count: 5"
        );
        assert_eq!(
            Error::UnsupportedPixelType("u32").to_string(),
            "unsupported pixel type: u32"
        );
        assert_eq!(
            Error::OutOfRange {
                value: 300.0,
                min: 0.0,
                max: 255.0
            }
            .to_string(),
            "value out of range: 300 not in [0, 255]"
        );
        assert_eq!(Error::EmptyInput.to_string(), "empty input");
        assert_eq!(Error::Codec("bad png".into()).to_string(), "codec: bad png");
    }

    #[test]
    fn io_from_std_io_error() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let e: Error = io.into();
        assert!(matches!(e, Error::Io(_)));
        assert!(e.to_string().contains("no such file"));
    }
}

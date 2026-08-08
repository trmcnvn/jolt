#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
use base64::engine::general_purpose::STANDARD_PAD_INDIFFERENT;
use base64::{DecodeError, Engine as _};

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn engine() -> &'static base64::engine::Simd {
    static ENGINE: std::sync::OnceLock<base64::engine::Simd> = std::sync::OnceLock::new();
    ENGINE.get_or_init(|| {
        base64::engine::Simd::standard(base64::engine::general_purpose::PAD_INDIFFERENT)
    })
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<u8>, DecodeError> {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    return engine().decode(bytes);

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    STANDARD_PAD_INDIFFERENT.decode(bytes)
}

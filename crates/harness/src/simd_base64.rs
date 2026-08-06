use base64::Engine as _;

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn engine() -> &'static base64::engine::Simd {
    static ENGINE: std::sync::OnceLock<base64::engine::Simd> = std::sync::OnceLock::new();
    ENGINE.get_or_init(|| {
        base64::engine::Simd::standard(base64::engine::general_purpose::PAD_INDIFFERENT)
    })
}

pub(crate) fn encode(bytes: &[u8]) -> String {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    return engine().encode(bytes);

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    base64::engine::general_purpose::STANDARD_PAD_INDIFFERENT.encode(bytes)
}

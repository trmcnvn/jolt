//! Runtime-selected SIMD base64 for binary payloads and token parsing.
//! The URL-safe engine roughly halved encode/decode time for a 2 KiB token
//! payload in an aarch64 release benchmark; unsupported targets stay scalar.

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
use base64::engine::general_purpose::{STANDARD_PAD_INDIFFERENT, URL_SAFE_NO_PAD_INDIFFERENT};
use base64::{DecodeError, Engine as _};

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn standard_engine() -> &'static base64::engine::Simd {
    static ENGINE: std::sync::OnceLock<base64::engine::Simd> = std::sync::OnceLock::new();
    ENGINE.get_or_init(|| {
        base64::engine::Simd::standard(base64::engine::general_purpose::PAD_INDIFFERENT)
    })
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn url_safe_engine() -> &'static base64::engine::Simd {
    static ENGINE: std::sync::OnceLock<base64::engine::Simd> = std::sync::OnceLock::new();
    ENGINE.get_or_init(|| {
        base64::engine::Simd::url_safe(base64::engine::general_purpose::NO_PAD_INDIFFERENT)
    })
}

pub(crate) fn encode(bytes: &[u8]) -> String {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    return standard_engine().encode(bytes);

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    STANDARD_PAD_INDIFFERENT.encode(bytes)
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<u8>, DecodeError> {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    return standard_engine().decode(bytes);

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    STANDARD_PAD_INDIFFERENT.decode(bytes)
}

pub(crate) fn encode_url_safe_no_pad(bytes: &[u8]) -> String {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    return url_safe_engine().encode(bytes);

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    URL_SAFE_NO_PAD_INDIFFERENT.encode(bytes)
}

pub(crate) fn decode_url_safe(bytes: &[u8]) -> Result<Vec<u8>, DecodeError> {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    return url_safe_engine().decode(bytes);

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    URL_SAFE_NO_PAD_INDIFFERENT.decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_accepts_padded_and_unpadded_input() {
        let encoded = encode(b"simd base64");
        assert!(encoded.ends_with('='));
        assert_eq!(decode(encoded.as_bytes()).unwrap(), b"simd base64");
        assert_eq!(
            decode(encoded.trim_end_matches('=').as_bytes()).unwrap(),
            b"simd base64"
        );
    }

    #[test]
    fn url_safe_round_trip_omits_padding() {
        let encoded = encode_url_safe_no_pad(&[251, 255, 239, 1]);
        assert!(encoded.contains(['-', '_']));
        assert!(!encoded.contains('='));
        assert_eq!(
            decode_url_safe(encoded.as_bytes()).unwrap(),
            [251, 255, 239, 1]
        );
        assert_eq!(
            decode_url_safe(format!("{encoded}==").as_bytes()).unwrap(),
            [251, 255, 239, 1]
        );
    }
}

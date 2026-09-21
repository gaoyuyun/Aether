use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use url::{form_urlencoded, Url};

/// RFC 7636 §4.1 要求 code_verifier 由密码学安全随机源生成，长度 43–128。
/// 64 字节原始熵经 base64url 无填充编码后是 86 个字符，落在允许区间内。
const PKCE_VERIFIER_ENTROPY_BYTES: usize = 64;
/// OAuth `state` / 浏览器绑定 nonce：32 字节熵，编码后 43 个字符。
const OAUTH_NONCE_ENTROPY_BYTES: usize = 32;

fn csprng_base64url(byte_len: usize) -> String {
    let mut buffer = vec![0u8; byte_len];
    // 操作系统 CSPRNG 不可用属于环境级故障，继续签发可预测的验证串只会掩盖问题。
    getrandom::fill(&mut buffer).expect("operating system CSPRNG must be available");
    URL_SAFE_NO_PAD.encode(buffer)
}

pub fn generate_oauth_nonce() -> String {
    csprng_base64url(OAUTH_NONCE_ENTROPY_BYTES)
}

pub fn generate_pkce_verifier() -> String {
    csprng_base64url(PKCE_VERIFIER_ENTROPY_BYTES)
}

/// 编码后的 nonce 长度：32 字节 base64url 无填充 = 43 个字符。
pub const OAUTH_NONCE_ENCODED_LEN: usize = 43;
/// 改用 CSPRNG 之前 nonce 是 64 位十六进制（两个 UUID 拼接后哈希）；发布期间仍有
/// 这种形状的 state 在飞行中，校验时继续接受。
const LEGACY_OAUTH_NONCE_HEX_LEN: usize = 64;

/// 判断一个字符串是否是 [`generate_oauth_nonce`] 产出的形状（或发布前的旧形状）。
/// 只看形状，不看来源；用于在解封 OAuth state 之前拒绝明显伪造的 nonce。
pub fn is_generated_oauth_nonce(nonce: &str) -> bool {
    let nonce = nonce.trim();
    if nonce.len() == OAUTH_NONCE_ENCODED_LEN {
        return nonce
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    }
    nonce.len() == LEGACY_OAUTH_NONCE_HEX_LEN
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn pkce_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

pub fn parse_oauth_callback_params(callback_url: &str) -> BTreeMap<String, String> {
    let mut merged = BTreeMap::new();
    let Ok(url) = Url::parse(callback_url.trim()) else {
        return merged;
    };

    for (key, value) in form_urlencoded::parse(url.query().unwrap_or_default().as_bytes()) {
        merged.insert(key.into_owned(), value.into_owned());
    }
    if let Some(fragment) = url.fragment() {
        for (key, value) in form_urlencoded::parse(fragment.trim_start_matches('#').as_bytes()) {
            merged.insert(key.into_owned(), value.into_owned());
        }
    }
    if let Some(code) = merged.get("code").cloned() {
        if let Some((code_part, state_part)) = code.split_once('#') {
            merged.insert("code".to_string(), code_part.to_string());
            if !merged.contains_key("state") && !state_part.is_empty() {
                let normalized_state = state_part
                    .strip_prefix("state=")
                    .unwrap_or(state_part)
                    .trim();
                if !normalized_state.is_empty() {
                    merged.insert("state".to_string(), normalized_state.to_string());
                }
            }
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::{
        generate_oauth_nonce, generate_pkce_verifier, is_generated_oauth_nonce,
        parse_oauth_callback_params, pkce_s256,
    };

    fn is_unreserved_pkce_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '~')
    }

    #[test]
    fn pkce_verifier_is_csprng_base64url_within_rfc7636_bounds() {
        let verifier = generate_pkce_verifier();
        assert_eq!(verifier.len(), 86);
        assert!(verifier.chars().all(is_unreserved_pkce_char));
        assert!(!verifier.contains('='));
        assert_ne!(verifier, generate_pkce_verifier());
    }

    #[test]
    fn oauth_nonce_shape_check_accepts_current_and_legacy_nonces() {
        assert!(is_generated_oauth_nonce(&generate_oauth_nonce()));
        assert!(is_generated_oauth_nonce(&"a".repeat(64)));
        assert!(!is_generated_oauth_nonce(&"A".repeat(64)));
        assert!(!is_generated_oauth_nonce(&"a".repeat(43).replace('a', "+")));
        assert!(!is_generated_oauth_nonce("short"));
        assert!(!is_generated_oauth_nonce(&"a".repeat(65)));
    }

    #[test]
    fn oauth_nonce_is_csprng_base64url() {
        let nonce = generate_oauth_nonce();
        assert_eq!(nonce.len(), 43);
        assert!(nonce.chars().all(is_unreserved_pkce_char));
        assert_ne!(nonce, generate_oauth_nonce());
    }

    #[test]
    fn parses_query_and_fragment_callback_params() {
        let params = parse_oauth_callback_params(
            "http://localhost/callback?code=query&state=old#code=fragment&scope=email",
        );
        assert_eq!(params.get("code").map(String::as_str), Some("fragment"));
        assert_eq!(params.get("state").map(String::as_str), Some("old"));
        assert_eq!(params.get("scope").map(String::as_str), Some("email"));
    }

    #[test]
    fn extracts_state_from_code_suffix() {
        let params =
            parse_oauth_callback_params("http://localhost/callback?code=abc%23state=state-1");
        assert_eq!(params.get("code").map(String::as_str), Some("abc"));
        assert_eq!(params.get("state").map(String::as_str), Some("state-1"));
    }

    #[test]
    fn pkce_s256_is_url_safe() {
        let value = pkce_s256("verifier");
        assert!(!value.contains('+'));
        assert!(!value.contains('/'));
        assert!(!value.contains('='));
    }
}

const MIN_SECRET_BYTES: usize = 32;
const TEST_JWT_SECRET: &str = "aether-test-jwt-secret-at-least-32-bytes";

fn is_known_insecure_secret(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "change-this-to-a-secure-random-string"
            | "change-this-to-another-secure-random-string"
            | "aether-rust-dev-jwt-secret"
            | "dev-encryption-key-do-not-use-in-production"
            | "changeme"
            | "password"
            | "secret"
    )
}

fn validate_secret_value(name: &str, value: Option<&str>) -> Result<String, String> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    let Some(value) = value else {
        return Err(format!("{name} must be configured"));
    };
    if is_known_insecure_secret(value) {
        return Err(format!(
            "{name} must not use a documented placeholder or development key"
        ));
    }
    if value.len() < MIN_SECRET_BYTES {
        return Err(format!(
            "{name} must contain at least {MIN_SECRET_BYTES} bytes"
        ));
    }
    Ok(value.to_string())
}

pub(crate) fn jwt_signing_secret() -> Result<String, String> {
    let configured = std::env::var("JWT_SECRET_KEY").ok();
    if configured
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        return validate_secret_value("JWT_SECRET_KEY", configured.as_deref());
    }

    #[cfg(test)]
    {
        return Ok(TEST_JWT_SECRET.to_string());
    }

    #[cfg(not(test))]
    validate_secret_value("JWT_SECRET_KEY", None)
}

#[doc(hidden)]
pub fn validate_runtime_secrets(
    database_configured: bool,
    encryption_key: Option<&str>,
) -> Result<(), std::io::Error> {
    jwt_signing_secret()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    if database_configured {
        validate_secret_value(
            "ENCRYPTION_KEY or AETHER_GATEWAY_DATA_ENCRYPTION_KEY",
            encryption_key,
        )
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_secret_value, MIN_SECRET_BYTES};

    #[test]
    fn rejects_missing_short_and_documented_secrets() {
        assert!(validate_secret_value("TEST_SECRET", None).is_err());
        assert!(validate_secret_value("TEST_SECRET", Some("too-short")).is_err());
        assert!(validate_secret_value(
            "TEST_SECRET",
            Some("change-this-to-a-secure-random-string")
        )
        .is_err());
    }

    #[test]
    fn accepts_random_length_secret() {
        let value = "x".repeat(MIN_SECRET_BYTES);
        assert_eq!(
            validate_secret_value("TEST_SECRET", Some(&value)).as_deref(),
            Ok(value.as_str())
        );
    }
}

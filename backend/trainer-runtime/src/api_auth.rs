use anyhow::{anyhow, bail, Result};
use hyper::header::{HeaderMap, AUTHORIZATION};
use subtle::ConstantTimeEq;

pub const SECRET_ENV: &str = "DECKY_TRAINER_SECRET";

/// Kept outside RuntimeState so authentication never waits for a memory action.
/// Deliberately has no Debug implementation: credentials must not enter logs.
pub struct ApiAuth {
    secret: String,
}

impl ApiAuth {
    pub fn from_env() -> Result<Self> {
        let value = std::env::var(SECRET_ENV);
        // Read before table Lua or background tasks can start child processes.
        std::env::remove_var(SECRET_ENV);
        // VarError::NotUnicode includes the value in its display text. Replace
        // it with a generic error rather than leaking credentials to stderr.
        let secret = value
            .map_err(|_| anyhow!("DECKY_TRAINER_SECRET is required to start the trainer API"))?;
        Self::new(secret)
    }

    pub fn new(secret: String) -> Result<Self> {
        if secret.len() != 64
            || !secret
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            bail!("DECKY_TRAINER_SECRET must contain 64 lowercase hexadecimal characters");
        }
        Ok(Self { secret })
    }

    pub fn authorized(&self, headers: &HeaderMap) -> bool {
        let mut values = headers.get_all(AUTHORIZATION).iter();
        let Some(value) = values.next() else {
            return false;
        };
        if values.next().is_some() {
            return false;
        }
        let Ok(value) = value.to_str() else {
            return false;
        };
        let Some((scheme, token)) = value.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("Bearer")
            && bool::from(self.secret.as_bytes().ct_eq(token.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    #[test]
    fn refuses_missing_malformed_and_incorrect_credentials() {
        let secret = "a".repeat(64);
        let auth = ApiAuth::new(secret.clone()).unwrap();
        let mut headers = HeaderMap::new();
        assert!(!auth.authorized(&headers));
        for value in [
            "Bearer".to_owned(),
            "Bearer ".to_owned(),
            format!("Basic {secret}"),
            format!("Bearer {}", "b".repeat(64)),
            format!("Bearer {}b", "a".repeat(63)),
            format!("Bearer {secret}extra"),
            format!("Bearer  {secret}"),
        ] {
            headers.insert(AUTHORIZATION, value.parse().unwrap());
            assert!(!auth.authorized(&headers));
        }
        headers.insert(AUTHORIZATION, format!("bEaReR {secret}").parse().unwrap());
        assert!(auth.authorized(&headers));
        headers.append(AUTHORIZATION, HeaderValue::from_static("Bearer wrong"));
        assert!(!auth.authorized(&headers));
    }

    #[test]
    fn refuses_invalid_configuration_without_echoing_it() {
        for secret in [
            "".to_owned(),
            "a".repeat(63),
            "G".repeat(64),
            "a".repeat(65),
        ] {
            let error = ApiAuth::new(secret).err().expect("invalid secret");
            assert_eq!(
                error.to_string(),
                "DECKY_TRAINER_SECRET must contain 64 lowercase hexadecimal characters"
            );
        }
    }
}

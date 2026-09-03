use axum::{
    extract::FromRequestParts,
    http::{StatusCode, request::Parts},
};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typ: Option<String>,
}

pub struct AuthUser {
    pub email: String,
}

pub fn create_token(
    email: &str,
    secret: &str,
    typ: Option<String>,
    expiration_secs: u64,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs();

    let claims = Claims {
        sub: email.to_owned(),
        exp: (now + expiration_secs) as usize,
        typ,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_ref()),
    )
}

pub fn verify_token(token: &str, secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_ref()),
        &Validation::default(),
    )
    .map(|data| data.claims)
}

impl FromRequestParts<std::sync::Arc<crate::api::AppState>> for AuthUser {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &std::sync::Arc<crate::api::AppState>,
    ) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get("Authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));

        let token = match auth_header {
            Some(token) => token,
            None => {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "Please log in to access bugs repository.",
                ));
            }
        };

        // In a real implementation this would come from the application state / config
        let secret = match std::env::var("JWT_SECRET") {
            Ok(s) => s,
            Err(_) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "JWT_SECRET is not configured on the server.",
                ));
            }
        };

        match verify_token(token, &secret) {
            Ok(claims) => {
                if claims.typ.as_deref() == Some("magic_link") {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        "Magic link tokens cannot be used as API tokens. Please exchange it for a session token.",
                    ));
                }
                Ok(AuthUser { email: claims.sub })
            }
            Err(_) => Err((
                StatusCode::UNAUTHORIZED,
                "Your session has expired or is invalid. Please log in again.",
            )),
        }
    }
}


pub struct OptionalAuthUser(pub Option<AuthUser>);

impl FromRequestParts<std::sync::Arc<crate::api::AppState>> for OptionalAuthUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<crate::api::AppState>,
    ) -> Result<Self, Self::Rejection> {
        match AuthUser::from_request_parts(parts, state).await {
            Ok(user) => Ok(OptionalAuthUser(Some(user))),
            Err(_) => Ok(OptionalAuthUser(None)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_create_and_verify_token() {
        let email = "test@example.com";
        let secret = "super_secret_key";

        let token = create_token(email, secret, Some("session".to_string()), 24 * 3600)
            .expect("Failed to create token");
        assert!(!token.is_empty());

        let claims = verify_token(&token, secret).expect("Failed to verify token");
        assert_eq!(claims.sub, email);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs() as usize;

        assert!(claims.exp > now);
        assert!(claims.exp <= now + 24 * 3600);
    }

    #[test]
    fn test_verify_token_invalid_secret() {
        let email = "test@example.com";
        let token = create_token(email, "secret1", None, 3600).unwrap();
        let result = verify_token(&token, "secret2");
        assert!(result.is_err());
    }
}


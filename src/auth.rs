use axum::{
    extract::FromRequestParts,
    http::{StatusCode, request::Parts},
};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;
use subtle::ConstantTimeEq;

/// The number of random bytes behind a local operator token.
const LOCAL_TOKEN_BYTES: usize = 32;

/// A credential that proves the caller can read a file owned by the user the
/// server runs as.
///
/// It exists because arriving on the loopback interface proves nothing: the
/// deployed topology puts a reverse proxy in front of a loopback bind, so a
/// request from the public internet reaches the server the same way a local one
/// does. Reading a 0600 file in the server's state directory is a claim that
/// caller cannot make, and it needs no configuration, which is what local
/// tooling requires.
///
/// The token is an authority, not an identity. It carries no email, so it can
/// never be consulted as an ACL entry and never stands in for a maintainer.
#[derive(Clone)]
pub struct LocalToken {
    secret: String,
}

impl LocalToken {
    /// Draws a fresh token from the system entropy source.
    ///
    /// fastrand is deliberately not used here. It is seeded predictably and is
    /// not a cryptographic generator, which is fine for worktree names and
    /// fatal for a credential.
    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0u8; LOCAL_TOKEN_BYTES];
        let mut file = std::fs::File::open("/dev/urandom")?;
        io::Read::read_exact(&mut file, &mut bytes)?;

        let secret = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        Ok(Self { secret })
    }

    /// Writes the token where a local tool can find it, readable only by the
    /// user that owns the server process.
    ///
    /// Any previous file is unlinked rather than truncated, because a mode is
    /// only applied to a file the open call creates: truncating would pour a
    /// fresh secret into whatever permissions the old file happened to carry.
    /// Creating exclusively also refuses to follow a symlink planted in the
    /// state directory, and leaves no window in which the file exists with
    /// wider access than it ends up with.
    pub fn write_to(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        writeln!(file, "{}", self.secret)
    }

    /// Reads a token a server published earlier.
    ///
    /// Contents that are not a token are rejected rather than carried forward,
    /// so a truncated or hand-edited file fails here with a readable error
    /// instead of as an unexplained 403 later.
    pub fn read_from(path: &Path) -> io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let secret = contents.trim();
        if !is_token_shaped(secret) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} does not hold a local token: expected {} hex characters",
                    path.display(),
                    LOCAL_TOKEN_BYTES * 2
                ),
            ));
        }

        Ok(Self {
            secret: secret.to_owned(),
        })
    }

    /// Whether the presented credential is this token.
    ///
    /// The comparison runs over SHA-256 digests so that neither the contents
    /// nor the length of the presented value steers how long the answer takes.
    pub fn matches(&self, presented: &str) -> bool {
        if !is_token_shaped(presented) {
            return false;
        }

        let expected = Sha256::digest(self.secret.as_bytes());
        let actual = Sha256::digest(presented.as_bytes());
        expected.ct_eq(&actual).into()
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }
}

/// Whether a string has the shape a local token takes.
///
/// A session JWT never does, which is what keeps the two credentials that share
/// the Authorization header from being mistaken for one another.
fn is_token_shaped(value: &str) -> bool {
    value.len() == LOCAL_TOKEN_BYTES * 2 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typ: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bug_access: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub email: String,
    pub iat: Option<usize>,
    pub sid: Option<String>,
    pub typ: Option<String>,
    pub max_bug_access: Option<String>,
}

impl AuthUser {
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            email: email.into(),
            iat: None,
            sid: None,
            typ: None,
            max_bug_access: None,
        }
    }
}

pub fn create_token(
    email: &str,
    secret: &str,
    typ: Option<String>,
    expiration_secs: u64,
) -> Result<String, jsonwebtoken::errors::Error> {
    create_token_with_session(email, secret, typ, expiration_secs, None, None)
}

pub fn create_api_token(
    email: &str,
    secret: &str,
    expiration_secs: u64,
    max_bug_access: Option<String>,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs();

    let claims = Claims {
        sub: email.to_owned(),
        exp: (now + expiration_secs) as usize,
        iat: Some(now as usize),
        sid: Some(format!("{:032x}", fastrand::u128(..))),
        typ: Some("api_token".to_string()),
        max_bug_access,
    };

    encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_ref()),
    )
}

pub fn create_token_with_session(
    email: &str,
    secret: &str,
    typ: Option<String>,
    expiration_secs: u64,
    iat: Option<usize>,
    sid: Option<String>,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs();

    let issued_at = iat.unwrap_or(now as usize);
    let session_id = sid.or_else(|| {
        if typ.as_deref() == Some("session") {
            Some(format!("{:032x}", fastrand::u128(..)))
        } else {
            None
        }
    });

    let claims = Claims {
        sub: email.to_owned(),
        exp: (now + expiration_secs) as usize,
        iat: Some(issued_at),
        sid: session_id,
        typ,
        max_bug_access: None,
    };

    encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_ref()),
    )
}

pub fn verify_token(token: &str, secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_ref()),
        &validation,
    )
    .map(|data| data.claims)
}

impl FromRequestParts<std::sync::Arc<crate::api::AppState>> for AuthUser {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<crate::api::AppState>,
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

        // Use the same configured secret as the login and authorization endpoints.
        let secret = match state
            .settings
            .server
            .jwt_secret
            .clone()
            .or_else(|| std::env::var("JWT_SECRET").ok())
        {
            Some(s) => s,
            None => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "JWT_SECRET is not configured on the server.",
                ));
            }
        };

        match verify_token(token, &secret) {
            Ok(claims) => {
                match claims.typ.as_deref() {
                    Some("session") | Some("api_token") => {}
                    _ => {
                        return Err((
                            StatusCode::UNAUTHORIZED,
                            "Invalid token type. Only session or API tokens are accepted.",
                        ));
                    }
                }
                Ok(AuthUser {
                    email: claims.sub,
                    iat: claims.iat,
                    sid: claims.sid,
                    typ: claims.typ,
                    max_bug_access: claims.max_bug_access,
                })
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

    #[test]
    fn test_session_token_generates_iat_and_sid() {
        let email = "test@example.com";
        let secret = "super_secret_key";
        let token = create_token(email, secret, Some("session".to_string()), 3600).unwrap();
        let claims = verify_token(&token, secret).unwrap();
        assert!(claims.iat.is_some());
        assert!(claims.sid.is_some());
        assert_eq!(claims.sid.as_ref().unwrap().len(), 32);
    }

    #[test]
    fn test_verify_token_rejects_unpinned_algorithm() {
        let email = "test@example.com";
        let secret = "super_secret_key";
        let claims = Claims {
            sub: email.to_string(),
            exp: 9999999999,
            iat: None,
            sid: None,
            typ: Some("session".to_string()),
            max_bug_access: None,
        };
        // Encode with HS384 instead of HS256
        let token = encode(
            &Header::new(jsonwebtoken::Algorithm::HS384),
            &claims,
            &EncodingKey::from_secret(secret.as_ref()),
        )
        .unwrap();

        let result = verify_token(&token, secret);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_and_verify_api_token_with_max_bug_access() {
        let email = "agent@example.com";
        let secret = "super_secret_key";
        let token = create_api_token(email, secret, 3600, Some("read".to_string())).unwrap();
        let claims = verify_token(&token, secret).unwrap();
        assert_eq!(claims.sub, email);
        assert_eq!(claims.typ.as_deref(), Some("api_token"));
        assert_eq!(claims.max_bug_access.as_deref(), Some("read"));
        assert!(claims.sid.as_deref().is_some_and(|s| s.len() == 32));
    }

    #[test]
    fn test_local_token_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".sashiko-local-token");

        let token = LocalToken::generate().unwrap();
        token.write_to(&path).unwrap();

        let read = LocalToken::read_from(&path).unwrap();
        assert!(token.matches(read.secret()));
        assert_eq!(read.secret().len(), 64);

        // Two tokens are independent, so a file from an earlier server run
        // authenticates nothing against a later one.
        let other = LocalToken::generate().unwrap();
        assert!(!token.matches(other.secret()));
    }

    /// The whole claim the token makes is that its reader shares the server's
    /// user, which is only true while nobody else can read the file.
    #[test]
    fn test_local_token_file_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".sashiko-local-token");

        // A pre-existing world-readable file must not survive the write.
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        LocalToken::generate().unwrap().write_to(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[test]
    fn test_local_token_rejects_a_file_that_is_not_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".sashiko-local-token");

        for contents in ["", "not a token", "abc123", &"z".repeat(64)] {
            std::fs::write(&path, contents).unwrap();
            assert!(
                LocalToken::read_from(&path).is_err(),
                "accepted {:?}",
                contents
            );
        }
    }

    /// Both credentials arrive in the Authorization header, so a session token
    /// must never be able to pass as the local one.
    #[test]
    fn test_local_token_does_not_match_a_session_jwt() {
        let token = LocalToken::generate().unwrap();
        let jwt = create_token(
            "test@example.com",
            "super_secret_key",
            Some("session".to_string()),
            3600,
        )
        .unwrap();

        assert!(!token.matches(&jwt));
        assert!(!token.matches(""));
    }
}

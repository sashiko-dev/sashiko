// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Short-term Amazon Bedrock API keys derived from IAM credentials.
//!
//! Mirrors the official `aws-bedrock-token-generator` libraries: a SigV4
//! presigned `CallWithBearerToken` URL, minus the scheme, base64-encoded.

use anyhow::{Context, Result, anyhow};
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::time::{Duration, SystemTime};

const TOKEN_PREFIX: &str = "bedrock-api-key-";
const HOST: &str = "bedrock.amazonaws.com";
const SIGNING_NAME: &str = "bedrock";
// Bedrock validates the key by replaying it as a POST; a GET signature never matches.
const METHOD: &str = "POST";
/// Longest lifetime Bedrock accepts for a short-term key.
const EXPIRES_IN: Duration = Duration::from_secs(12 * 3600);
/// Renew this long before the key, or the credentials it embeds, expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(3600);

/// A short-term key and the time after which it should be replaced.
pub(crate) struct BedrockToken {
    pub(crate) token: String,
    pub(crate) refresh_at: SystemTime,
}

pub(crate) fn presign_token(creds: &Credentials, region: &str, now: SystemTime) -> Result<String> {
    let url = format!("https://{HOST}/?Action=CallWithBearerToken");
    let mut settings = SigningSettings::default();
    settings.signature_location = SignatureLocation::QueryParams;
    settings.expires_in = Some(EXPIRES_IN);

    let identity = creds.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(SIGNING_NAME)
        .time(now)
        .settings(settings)
        .build()
        .context("invalid signing parameters")?
        .into();
    let request = SignableRequest::new(
        METHOD,
        url.as_str(),
        std::iter::once(("host", HOST)),
        SignableBody::Bytes(&[]),
    )
    .context("invalid presign request")?;
    let (instructions, _) = sign(request, &params)
        .context("presigning failed")?
        .into_parts();

    let mut presigned = url;
    for (name, value) in instructions.params() {
        presigned.push('&');
        presigned.push_str(name);
        presigned.push('=');
        presigned.extend(url::form_urlencoded::byte_serialize(value.as_bytes()));
    }
    let presigned = presigned
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("unexpected presigned url"))?;

    Ok(format!(
        "{TOKEN_PREFIX}{}",
        STANDARD.encode(format!("{presigned}&Version=1"))
    ))
}

/// A key is only as valid as the credentials it was signed with, so
/// temporary (SSO, STS) credentials shorten its useful life.
fn refresh_at(creds: &Credentials, now: SystemTime) -> SystemTime {
    let key_expiry = now + EXPIRES_IN;
    let expiry = creds
        .expiry()
        .map_or(key_expiry, |creds_expiry| creds_expiry.min(key_expiry));
    expiry
        .checked_sub(REFRESH_MARGIN)
        .filter(|at| *at > now)
        .unwrap_or_else(|| {
            // Credentials expiring within the margin: use them, but renew
            // halfway to their expiry rather than on every request.
            now + expiry.duration_since(now).unwrap_or_default() / 2
        })
}

pub(crate) async fn generate_token(region: &str) -> Result<BedrockToken> {
    let sdk_config = aws_config::from_env()
        .region(aws_config::Region::new(region.to_string()))
        .load()
        .await;
    let provider = sdk_config
        .credentials_provider()
        .ok_or_else(|| anyhow!("no AWS credentials provider configured"))?;
    let creds = provider
        .provide_credentials()
        .await
        .context("failed to load AWS credentials")?;
    let now = SystemTime::now();
    Ok(BedrockToken {
        token: presign_token(&creds, region, now)?,
        refresh_at: refresh_at(&creds, now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn creds(session_token: Option<&str>) -> Credentials {
        Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token.map(str::to_string),
            None,
            "test",
        )
    }

    fn decode(token: &str) -> String {
        let encoded = token.strip_prefix(TOKEN_PREFIX).expect("token prefix");
        String::from_utf8(STANDARD.decode(encoded).expect("valid base64")).expect("utf-8")
    }

    fn fixed_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn presigns_bearer_token_url() {
        let url = decode(&presign_token(&creds(None), "eu-north-1", fixed_time()).unwrap());

        assert!(url.starts_with("bedrock.amazonaws.com/?"), "{url}");
        assert!(url.ends_with("&Version=1"), "{url}");
        assert!(url.contains("Action=CallWithBearerToken"), "{url}");
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"), "{url}");
        assert!(url.contains("X-Amz-Expires=43200"), "{url}");
        assert!(
            url.contains(
                "X-Amz-Credential=AKIDEXAMPLE%2F20231114%2Feu-north-1%2Fbedrock%2Faws4_request"
            ),
            "{url}"
        );
        assert!(!url.contains("X-Amz-Security-Token"), "{url}");
    }

    /// Reference from botocore's `SigV4QueryAuth`, which the official token generators use.
    #[test]
    fn signature_matches_botocore_post_presign() {
        let url = decode(&presign_token(&creds(None), "eu-north-1", fixed_time()).unwrap());

        assert!(url.contains("X-Amz-Date=20231114T221320Z"), "{url}");
        assert!(
            url.contains(
                "X-Amz-Signature=75dc42147fdefeeed63ebefbfccb37f9ae0812f34451a4096e19043fca9315bf"
            ),
            "{url}"
        );
    }

    #[test]
    fn presigning_includes_session_token() {
        let url = decode(
            &presign_token(&creds(Some("session/token+1")), "us-east-1", fixed_time()).unwrap(),
        );

        assert!(
            url.contains("X-Amz-Security-Token=session%2Ftoken%2B1"),
            "{url}"
        );
        assert!(url.contains("%2Fus-east-1%2Fbedrock%2F"), "{url}");
    }

    #[test]
    fn refresh_follows_the_earlier_of_key_and_credential_expiry() {
        let now = fixed_time();
        let hour = Duration::from_secs(3600);
        let expiring = |after: Duration| {
            let mut creds = creds(Some("session"));
            *creds.expiry_mut() = Some(now + after);
            creds
        };

        assert_eq!(refresh_at(&creds(None), now), now + 11 * hour);
        assert_eq!(refresh_at(&expiring(6 * hour), now), now + 5 * hour);
        assert_eq!(refresh_at(&expiring(24 * hour), now), now + 11 * hour);
        assert_eq!(
            refresh_at(&expiring(Duration::from_secs(1200)), now),
            now + Duration::from_secs(600)
        );
    }
}

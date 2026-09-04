//! bex sign-in via OpenID Connect.
//!
//! bex's identity plane is Ory Hydra (OAuth2/OIDC) + Kratos (accounts), and its
//! first-party desktop/CLI clients authenticate with the **OAuth 2.0 Device
//! Authorization Grant** (RFC 8628) using a pre-registered, secretless public
//! client. This module drives that same flow so the editor signs in against the
//! real bex IdP with no backend changes:
//!
//! 1. Discover endpoints from `{issuer}/.well-known/openid-configuration`.
//! 2. POST the device authorization request → `user_code` + `verification_uri`.
//! 3. Send the user to the verification URL (browser) to approve.
//! 4. Poll the token endpoint until approval → OIDC tokens.
//! 5. Derive a stable numeric user id from the `id_token`.
//!
//! Defaults target bex (issuer `oauth.bex.co`, the seeded public CLI client);
//! all three knobs are overridable via `BEX_OIDC_ISSUER`, `BEX_OIDC_CLIENT_ID`,
//! and `BEX_OIDC_SCOPES`.

use anyhow::{Context as _, Result, bail};
use base64::prelude::*;
use futures::AsyncReadExt as _;
use http_client::{
    AsyncBody, HttpClient,
    http::{Method, Request},
};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use std::{future::Future, sync::Arc, time::Duration};
use url::Url;

/// The seeded, secretless public client bex registers for its desktop/editor
/// surface (device grant, same scopes as the CLI but a distinct client so token
/// audience, telemetry, and revocation are decoupled). Seeded by
/// `scripts/auth-bootstrap-client.sh` in the bex platform repo.
const DEFAULT_CLIENT_ID: &str = "bex-desktop";

/// Configuration for the sign-in flow.
pub struct OidcConfig {
    /// The OIDC issuer (e.g. `https://oauth.bex.co`). Discovery reads
    /// `{issuer}/.well-known/openid-configuration`.
    pub issuer: Url,
    /// The public OAuth client id. The device grant needs no client secret.
    pub client_id: String,
    /// Requested scopes. Must include `openid` for an `id_token`; include
    /// `offline_access` for a refresh token.
    pub scopes: Vec<String>,
}

impl OidcConfig {
    /// Read the configuration from the environment, defaulting to bex's IdP.
    ///
    /// The issuer defaults to the `oauth.` subdomain of the app's `server_url`
    /// (so a stock bex build resolves `https://oauth.bex.co`). Overrides:
    /// `BEX_OIDC_ISSUER`, `BEX_OIDC_CLIENT_ID`, `BEX_OIDC_SCOPES` (space-list).
    pub fn from_env(server_url: &str) -> Result<Self> {
        let issuer = std::env::var("BEX_OIDC_ISSUER").unwrap_or_else(|_| default_issuer(server_url));
        let issuer = Url::parse(&issuer).context("invalid BEX_OIDC_ISSUER / server_url")?;
        let client_id =
            std::env::var("BEX_OIDC_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string());
        let scopes = std::env::var("BEX_OIDC_SCOPES")
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|_| {
                ["openid", "offline_access", "bex.read", "bex.write"]
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            });
        Ok(Self {
            issuer,
            client_id,
            scopes,
        })
    }
}

/// Derive bex's OIDC issuer from the app server URL by prefixing the `oauth.`
/// subdomain (`https://bex.co` -> `https://oauth.bex.co`).
fn default_issuer(server_url: &str) -> String {
    if let Ok(url) = Url::parse(server_url) {
        if let Some(host) = url.host_str() {
            let host = if host.starts_with("oauth.") {
                host.to_string()
            } else {
                format!("oauth.{host}")
            };
            return format!("{}://{host}", url.scheme());
        }
    }
    "https://oauth.bex.co".to_string()
}

/// What to show the user so they can approve the device on another surface.
pub struct DevicePrompt {
    pub user_code: String,
    pub verification_uri: String,
    /// A verification URL with the `user_code` pre-filled, when the provider
    /// supplies one — open this to spare the user typing the code.
    pub verification_uri_complete: Option<String>,
}

/// Tokens obtained from a successful sign-in.
pub struct AuthTokens {
    /// Numeric user id derived from the `id_token`.
    pub user_id: u64,
    /// The provider access token — the session credential against the backend.
    pub access_token: String,
    /// Refresh token, present when `offline_access` was granted.
    pub refresh_token: Option<String>,
}

/// Run the Device Authorization Grant.
///
/// `prompt` is invoked once with the verification URL / user code so the caller
/// can open the browser and/or display the code. `sleep` yields for the given
/// duration between token polls (pass the host's async timer).
pub async fn authenticate<Sleep, SleepFut>(
    http: Arc<dyn HttpClient>,
    config: OidcConfig,
    prompt: impl FnOnce(&DevicePrompt),
    sleep: Sleep,
) -> Result<AuthTokens>
where
    Sleep: Fn(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
{
    let metadata = discover(&http, &config.issuer).await?;

    let device: DeviceAuthorizationResponse = post_form(
        &http,
        &metadata.device_authorization_endpoint,
        &[
            ("client_id", config.client_id.as_str()),
            ("scope", &config.scopes.join(" ")),
        ],
    )
    .await
    .context("device authorization request failed")?;

    prompt(&DevicePrompt {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        verification_uri_complete: device.verification_uri_complete.clone(),
    });

    let mut interval = Duration::from_secs(device.interval.unwrap_or(5).max(1));
    // Bound the wait to the device code's lifetime (default 10 min), plus a
    // couple of extra polls of slack.
    let expires_in = device.expires_in.unwrap_or(600);
    let max_polls = expires_in / interval.as_secs().max(1) + 2;

    for _ in 0..max_polls {
        sleep(interval).await;
        match poll_token(
            &http,
            &metadata.token_endpoint,
            &config.client_id,
            &device.device_code,
        )
        .await?
        {
            PollOutcome::Tokens(tokens) => {
                let id_token = tokens
                    .id_token
                    .as_deref()
                    .context("token response is missing an id_token; request the `openid` scope")?;
                return Ok(AuthTokens {
                    user_id: user_id_from_id_token(id_token)?,
                    access_token: tokens.access_token,
                    refresh_token: tokens.refresh_token,
                });
            }
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval += Duration::from_secs(5),
        }
    }

    bail!("device sign-in timed out before it was approved")
}

/// Validate an access token against the provider's `userinfo` endpoint.
///
/// Used to check a persisted credential without re-running the browser flow:
/// `200` means the token is live, `401`/`403` means it is not. Any other status
/// is a transport/transient failure and surfaces as an error so the caller can
/// tell "signed out" apart from "couldn't reach the IdP".
pub async fn validate_token(
    http: Arc<dyn HttpClient>,
    config: &OidcConfig,
    access_token: &str,
) -> Result<bool> {
    let metadata = discover(&http, &config.issuer).await?;
    let userinfo = metadata.userinfo_endpoint.unwrap_or_else(|| {
        format!("{}/userinfo", config.issuer.as_str().trim_end_matches('/'))
    });
    let request = Request::builder()
        .method(Method::GET)
        .uri(&userinfo)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .body(AsyncBody::default())?;
    let response = http.send(request).await.context("userinfo request failed")?;
    match response.status().as_u16() {
        200..=299 => Ok(true),
        401 | 403 => Ok(false),
        other => bail!("userinfo returned unexpected status {other}"),
    }
}

#[derive(Deserialize)]
struct ProviderMetadata {
    device_authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
}

enum PollOutcome {
    Tokens(TokenResponse),
    Pending,
    SlowDown,
}

async fn discover(http: &Arc<dyn HttpClient>, issuer: &Url) -> Result<ProviderMetadata> {
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    );
    let request = Request::builder()
        .method(Method::GET)
        .uri(&discovery_url)
        .header("Accept", "application/json")
        .body(AsyncBody::default())?;

    let mut response = http
        .send(request)
        .await
        .context("OIDC discovery request failed")?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    if !response.status().is_success() {
        bail!(
            "OIDC discovery failed with status {}: {body}",
            response.status()
        );
    }
    serde_json::from_str(&body).context("failed to parse the OIDC discovery document")
}

/// Poll the token endpoint once with the device grant, mapping the RFC 8628
/// pending/slow-down signals to control flow.
async fn poll_token(
    http: &Arc<dyn HttpClient>,
    token_endpoint: &str,
    client_id: &str,
    device_code: &str,
) -> Result<PollOutcome> {
    let body = form(&[
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code",
        ),
        ("device_code", device_code),
        ("client_id", client_id),
    ]);
    let request = Request::builder()
        .method(Method::POST)
        .uri(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(AsyncBody::from(body.into_bytes()))?;

    let mut response = http.send(request).await.context("token poll failed")?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;

    if response.status().is_success() {
        let tokens = serde_json::from_str(&text).context("failed to parse the token response")?;
        return Ok(PollOutcome::Tokens(tokens));
    }

    let error = serde_json::from_str::<TokenErrorResponse>(&text)
        .map(|e| e.error)
        .unwrap_or_else(|_| format!("HTTP {}: {text}", response.status()));
    match error.as_str() {
        "authorization_pending" => Ok(PollOutcome::Pending),
        "slow_down" => Ok(PollOutcome::SlowDown),
        "access_denied" => bail!("sign-in was denied"),
        "expired_token" => bail!("the device code expired before it was approved"),
        other => bail!("device token poll failed: {other}"),
    }
}

async fn post_form<T: DeserializeOwned>(
    http: &Arc<dyn HttpClient>,
    endpoint: &str,
    params: &[(&str, &str)],
) -> Result<T> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(AsyncBody::from(form(params).into_bytes()))?;

    let mut response = http.send(request).await?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    if !response.status().is_success() {
        bail!("request failed with status {}: {text}", response.status());
    }
    serde_json::from_str(&text).context("failed to parse response")
}

fn form(params: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().copied())
        .finish()
}

/// Derive the numeric user id from the id_token's claims.
///
/// The `id_token` signature is not verified here: the `access_token` is the
/// credential the backend re-validates. bex/Kratos subjects are UUIDs, but Zed's
/// `Credentials.user_id` is a `u64`, so when no numeric claim is present we
/// derive a stable id from the `sub` (SHA-256 → first 8 bytes). Deterministic
/// per subject; only a local identifier, never sent as an auth credential.
fn user_id_from_id_token(id_token: &str) -> Result<u64> {
    let payload = id_token
        .split('.')
        .nth(1)
        .context("malformed id_token (expected a JWT with three segments)")?;
    let decoded = BASE64_URL_SAFE_NO_PAD
        .decode(payload)
        .context("failed to base64url-decode the id_token payload")?;
    let claims: serde_json::Value =
        serde_json::from_slice(&decoded).context("failed to parse id_token claims")?;

    // Prefer an explicit numeric id if bex ever emits one.
    for key in ["user_id", "zed_user_id"] {
        match claims.get(key) {
            Some(serde_json::Value::Number(number)) => {
                if let Some(id) = number.as_u64() {
                    return Ok(id);
                }
            }
            Some(serde_json::Value::String(value)) => {
                if let Ok(id) = value.parse::<u64>() {
                    return Ok(id);
                }
            }
            _ => {}
        }
    }

    let sub = claims
        .get("sub")
        .and_then(|value| value.as_str())
        .context("id_token has no `sub` claim")?;
    Ok(stable_u64(sub))
}

/// A deterministic, non-zero `u64` derived from a string (SHA-256, first 8
/// bytes). Non-zero because `user_id == 0` is treated as "signed out".
fn stable_u64(value: &str) -> u64 {
    let digest = Sha256::digest(value.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes) | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_token(payload: &[u8]) -> String {
        format!("aaa.{}.bbb", BASE64_URL_SAFE_NO_PAD.encode(payload))
    }

    #[test]
    fn prefers_explicit_numeric_user_id_claim() {
        let token = id_token(br#"{"sub":"a-uuid","user_id":7}"#);
        assert_eq!(user_id_from_id_token(&token).unwrap(), 7);
    }

    #[test]
    fn derives_stable_nonzero_u64_from_uuid_sub() {
        let token = id_token(br#"{"sub":"f25579e9-97ce-436a-805d-771975cb8fd2"}"#);
        let first = user_id_from_id_token(&token).unwrap();
        let second = user_id_from_id_token(&token).unwrap();
        assert_eq!(first, second, "derivation must be deterministic");
        assert_ne!(first, 0);
    }

    #[test]
    fn distinct_subjects_derive_distinct_ids() {
        let a = user_id_from_id_token(&id_token(br#"{"sub":"alice"}"#)).unwrap();
        let b = user_id_from_id_token(&id_token(br#"{"sub":"bob"}"#)).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn errors_without_sub() {
        assert!(user_id_from_id_token(&id_token(br#"{"name":"x"}"#)).is_err());
    }

    #[test]
    fn default_issuer_prepends_oauth_subdomain() {
        assert_eq!(default_issuer("https://bex.co"), "https://oauth.bex.co");
        assert_eq!(default_issuer("https://bex.co/"), "https://oauth.bex.co");
        assert_eq!(default_issuer("https://oauth.bex.co"), "https://oauth.bex.co");
    }
}

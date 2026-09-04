//! bex sign-in via OpenID Connect.
//!
//! bex's identity plane is Ory Hydra (OAuth2/OIDC) + Kratos (accounts). The
//! editor is a desktop GUI app, so it uses the standard native-app flow —
//! OAuth 2.0 Authorization Code + PKCE (RFC 7636) with an RFC 8252 loopback
//! redirect — not the device grant (that is for browserless / input-constrained
//! surfaces like the CLI). The user approves in the browser and is redirected
//! straight back to a loopback listener; with the first-party client's
//! `skip_consent`, an already-signed-in browser session lands back with no
//! extra clicks.
//!
//! The flow, entirely client-side:
//! 1. Discover endpoints from `{issuer}/.well-known/openid-configuration`.
//! 2. Generate a PKCE verifier/challenge and an anti-CSRF `state`.
//! 3. Start a loopback callback server ([`oauth_callback_server`]).
//! 4. Open the browser to the authorization endpoint.
//! 5. Receive the `code` on the loopback redirect, verifying `state`.
//! 6. Exchange the `code` (+ PKCE verifier) at the token endpoint for tokens.
//! 7. Derive a stable numeric user id from the returned `id_token`.
//!
//! Defaults target bex (issuer `oauth.bex.co`, the first-party `bex-desktop`
//! client); all overridable via `BEX_OIDC_ISSUER`, `BEX_OIDC_CLIENT_ID`,
//! `BEX_OIDC_SCOPES`.

use anyhow::{Context as _, Result, bail};
use base64::prelude::*;
use futures::AsyncReadExt as _;
use http_client::{
    AsyncBody, HttpClient,
    http::{Method, Request},
};
use oauth_callback_server::start_oauth_callback_server;
use rand::RngCore as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use url::Url;

/// The first-party public client bex registers for the desktop/editor surface
/// (Authorization Code + PKCE, loopback redirect). Seeded by
/// `scripts/auth-bootstrap-client.sh` in the bex platform repo.
const DEFAULT_CLIENT_ID: &str = "bex-desktop";

/// Configuration for the sign-in flow.
pub struct OidcConfig {
    /// The OIDC issuer (e.g. `https://oauth.bex.co`). Discovery reads
    /// `{issuer}/.well-known/openid-configuration`.
    pub issuer: Url,
    /// The public OAuth client id. PKCE means no client secret is required.
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

/// Tokens obtained from a successful sign-in.
pub struct AuthTokens {
    /// Numeric user id derived from the `id_token` claims.
    pub user_id: u64,
    /// The provider access token — the session credential against the backend.
    pub access_token: String,
    /// Refresh token, present when `offline_access` was granted.
    pub refresh_token: Option<String>,
}

/// The signed-in user's profile, read from the provider's `userinfo` claims.
pub struct UserProfile {
    pub username: String,
    pub name: Option<String>,
    pub avatar_url: String,
}

/// Run the Authorization Code + PKCE sign-in flow.
///
/// `open_url` is invoked with the authorization URL; the caller opens it in the
/// user's browser (which, in gpui, must happen on the main thread). The returned
/// future resolves once the browser redirects the `code` back to the loopback
/// listener and it is exchanged for tokens.
pub async fn authenticate(
    http: Arc<dyn HttpClient>,
    config: OidcConfig,
    open_url: impl FnOnce(String),
) -> Result<AuthTokens> {
    log::info!("bex_auth: discovering issuer {}", config.issuer);
    let metadata = discover(&http, &config.issuer).await?;

    let code_verifier = random_token();
    let code_challenge = pkce_challenge(&code_verifier);
    let state = random_token();

    let (redirect_uri, callback) =
        start_oauth_callback_server().context("failed to start the OAuth callback server")?;

    let mut authorization_url =
        Url::parse(&metadata.authorization_endpoint).context("invalid authorization_endpoint")?;
    authorization_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &config.scopes.join(" "))
        .append_pair("state", &state)
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256");

    log::info!("bex_auth: opening browser to authorize (redirect {redirect_uri})");
    open_url(authorization_url.to_string());

    let params = callback
        .await
        .context("OAuth callback channel closed before a redirect was received")?
        .context("OAuth authorization failed")?;

    // `state` is a fresh high-entropy value from this flow; a plain compare is
    // sufficient (it is never persisted or attacker-chosen).
    if params.state != state {
        bail!("OAuth state mismatch; possible CSRF — aborting sign-in");
    }

    let tokens = exchange_code(
        &http,
        &metadata.token_endpoint,
        &config.client_id,
        &redirect_uri,
        &params.code,
        &code_verifier,
    )
    .await?;

    let id_token = tokens
        .id_token
        .as_deref()
        .context("token response is missing an id_token; request the `openid` scope")?;
    Ok(AuthTokens {
        user_id: user_id_from_id_token(id_token)?,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
    })
}

/// Validate an access token against the provider's `userinfo` endpoint.
///
/// Checks a persisted credential without re-running the browser flow: `200`
/// means the token is live, `401`/`403` means it is not. Any other status is a
/// transport/transient failure and surfaces as an error so the caller can tell
/// "signed out" apart from "couldn't reach the IdP".
pub async fn validate_token(
    http: Arc<dyn HttpClient>,
    config: &OidcConfig,
    access_token: &str,
) -> Result<bool> {
    log::info!("bex_auth: validating token against {} userinfo", config.issuer);
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

/// Fetch the signed-in user's profile from the provider's `userinfo` endpoint.
pub async fn fetch_user(
    http: Arc<dyn HttpClient>,
    config: &OidcConfig,
    access_token: &str,
) -> Result<UserProfile> {
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
    let mut response = http.send(request).await.context("userinfo request failed")?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    if !response.status().is_success() {
        bail!(
            "userinfo request failed with status {}: {body}",
            response.status()
        );
    }
    let claims: UserInfoClaims =
        serde_json::from_str(&body).context("failed to parse userinfo claims")?;
    let email = claims.email.clone();
    let username = claims
        .preferred_username
        .or(claims.email)
        .or(claims.sub)
        .unwrap_or_else(|| "bex-user".to_string());
    // bex's Hydra `userinfo` has no `picture` claim, so fall back to a
    // deterministic Gravatar identicon keyed on the email (or username). Keeps
    // the signed-in UI from trying to load an empty avatar URL.
    let avatar_url = claims.picture.filter(|p| !p.is_empty()).unwrap_or_else(|| {
        let key = email.unwrap_or_else(|| username.clone()).to_lowercase();
        let digest = Sha256::digest(key.as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("https://www.gravatar.com/avatar/{hex}?d=identicon&s=128")
    });
    Ok(UserProfile {
        username,
        name: claims.name,
        avatar_url,
    })
}

#[derive(Deserialize)]
struct ProviderMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
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
struct UserInfoClaims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    picture: Option<String>,
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

async fn exchange_code(
    http: &Arc<dyn HttpClient>,
    token_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
) -> Result<TokenResponse> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", client_id)
        .append_pair("code_verifier", code_verifier)
        .finish();

    let request = Request::builder()
        .method(Method::POST)
        .uri(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(AsyncBody::from(body.into_bytes()))?;

    let mut response = http.send(request).await.context("token request failed")?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    if !response.status().is_success() {
        bail!(
            "token request failed with status {}: {body}",
            response.status()
        );
    }
    serde_json::from_str(&body).context("failed to parse the token response")
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

/// Generate a high-entropy URL-safe token, used both as the PKCE verifier
/// (RFC 7636 §4.1 requires 43-128 unreserved characters — 32 random bytes
/// base64url-encode to 43) and as the anti-CSRF `state`.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    BASE64_URL_SAFE_NO_PAD.encode(bytes)
}

/// Compute the S256 PKCE challenge from a verifier (RFC 7636 §4.2):
/// `base64url(sha256(verifier))`, no padding.
fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    BASE64_URL_SAFE_NO_PAD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_token(payload: &[u8]) -> String {
        format!("aaa.{}.bbb", BASE64_URL_SAFE_NO_PAD.encode(payload))
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_appendix_b() {
        // The worked example from RFC 7636, Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_token_is_pkce_length() {
        // 32 bytes base64url (no pad) => 43 chars, within RFC 7636's 43-128 and
        // well over Hydra's 8-char minimum for `state`.
        let token = random_token();
        assert_eq!(token.len(), 43);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
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

//! OpenAI "Sign in with ChatGPT" for plan usage: an OpenID Connect
//! authorization-code flow with PKCE and dynamic client registration.
//!
//! The first sign-in sends `client_id=dynamic_agent_client` with an
//! `agent_name_hint`; the callback returns the `client_id` OpenAI issued,
//! which later sign-ins and every refresh use. Each request also names this
//! kernel's `ext_agent_host_id`, a UUIDv4 kept beside the grants.
//!
//! The redirect is `http://127.0.0.1:1455/auth/callback`. Nothing listens
//! there: the player approves in a browser, the browser fails to load the
//! redirect, and the player pastes that address back (`kj backend login
//! <name> --callback <url>`). That works when the kernel runs on another
//! host.
//!
//! The ID token's `iss`, `aud`, `nonce` and `exp` claims are checked; its
//! signature is not. The token comes straight from the token endpoint over
//! TLS in the code flow, which OpenID Connect Core 3.1.3.7 (6) accepts in
//! place of a signature check.

use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::credentials::{Grant, now};

/// The OAuth endpoints a sign-in uses. [`OPENAI`] in production; tests aim
/// them at a local server.
#[derive(Debug, Clone)]
pub struct Issuer {
    pub authorize_url: String,
    pub token_url: String,
    /// The `iss` an ID token must carry.
    pub issuer: String,
}

impl Issuer {
    pub fn openai() -> Self {
        Self {
            authorize_url: "https://auth.openai.com/api/accounts/authorize".into(),
            token_url: "https://auth.openai.com/api/accounts/oauth/token".into(),
            issuer: "https://auth.openai.com".into(),
        }
    }
}

/// The `client_id` a first sign-in sends to be issued one of its own.
const BOOTSTRAP_CLIENT_ID: &str = "dynamic_agent_client";
/// The name shown on the consent page for a new registration.
const AGENT_NAME: &str = "kaijutsu";
const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The scope without which a grant cannot spend plan usage.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// The `resource` sent on authorize, exchange and refresh.
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// The redirect URI. Only its port may differ between sign-ins.
pub const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";

/// How long a token request may take.
pub const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One sign-in attempt's secrets. Fresh per attempt and held in memory only.
#[derive(Clone)]
pub struct Attempt {
    pub verifier: String,
    pub state: String,
    pub nonce: String,
}

impl std::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Attempt { .. }")
    }
}

/// 32 random bytes, base64url without padding.
fn random_token() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The S256 PKCE challenge for `verifier`.
pub fn code_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

impl Attempt {
    /// A fresh attempt with new random secrets.
    pub fn fresh() -> Self {
        Self { verifier: random_token(), state: random_token(), nonce: random_token() }
    }
}

/// The address the player opens to sign in. `client_id` is the one an
/// earlier sign-in was issued; `None` registers a new client.
pub fn authorize_url(issuer: &Issuer, attempt: &Attempt, client_id: Option<&str>, host_id: &str) -> String {
    let mut url = url::Url::parse(&issuer.authorize_url).expect("the authorize URL parses");
    {
        let mut q = url.query_pairs_mut();
        match client_id {
            Some(id) => {
                q.append_pair("client_id", id);
            }
            None => {
                q.append_pair("client_id", BOOTSTRAP_CLIENT_ID);
                q.append_pair("agent_name_hint", AGENT_NAME);
            }
        }
        q.append_pair("ext_agent_host_id", host_id);
        q.append_pair("response_type", "code");
        q.append_pair("redirect_uri", REDIRECT_URI);
        q.append_pair("scope", SCOPES);
        q.append_pair("resource", RESOURCE);
        q.append_pair("state", &attempt.state);
        q.append_pair("nonce", &attempt.nonce);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("code_challenge", &code_challenge(&attempt.verifier));
    }
    url.into()
}

/// What a checked callback carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackCode {
    pub code: String,
    /// The issued client id, on a new registration.
    pub client_id: Option<String>,
}

/// Check the address the browser was sent to against `attempt`. `pasted` is
/// the whole redirect address or just its query string.
///
/// `state` is checked first, so a callback from another attempt is refused
/// before anything else in it is read.
pub fn check_callback(attempt: &Attempt, pasted: &str, known_client_id: Option<&str>) -> Result<CallbackCode, String> {
    let pasted = pasted.trim();
    let query = match pasted.split_once('?') {
        Some((_, query)) => query,
        None => pasted,
    };
    let query = query.split('#').next().unwrap_or(query);
    let (mut code, mut state, mut client_id, mut error, mut description) = (None, None, None, None, None);
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        let v = Some(v.into_owned());
        match k.as_ref() {
            "code" => code = v,
            "state" => state = v,
            "client_id" => client_id = v,
            "error" => error = v,
            "error_description" => description = v,
            _ => {}
        }
    }
    if state.as_deref() != Some(attempt.state.as_str()) {
        return Err("the pasted address is not from this sign-in (its state does not match); \
                    run `kj backend login <name>` again and paste the address the browser ends on"
            .into());
    }
    if let Some(error) = error {
        let description = description.map(|d| format!(" ({d})")).unwrap_or_default();
        return Err(format!("OpenAI refused the sign-in: {error}{description}"));
    }
    let code = code.ok_or("the pasted address carries no code; paste the full address the browser ends on")?;
    if let (Some(issued), Some(known)) = (client_id.as_deref(), known_client_id)
        && issued != known
    {
        return Err("the sign-in returned a different client_id than this backend's earlier sign-in; \
                    run `kj backend logout <name>`, then sign in again"
            .into());
    }
    Ok(CallbackCode { code, client_id })
}

/// The token endpoint's reply to an exchange or a refresh.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// Why a token request failed.
#[derive(Debug)]
pub enum TokenError {
    /// The grant can no longer be refreshed; only a new sign-in fixes it.
    Dead(String),
    /// Anything else: the endpoint was unreachable, or answered an error a
    /// later attempt may not.
    Other(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dead(m) | Self::Other(m) => f.write_str(m),
        }
    }
}

/// OAuth `error` codes that mean the grant itself is gone.
fn is_dead(code: &str) -> bool {
    matches!(
        code,
        "invalid_grant"
            | "invalid_client"
            | "unauthorized_client"
            | "invalid_refresh_token"
            | "token_expired"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused"
    )
}

async fn post_form(http: &reqwest::Client, url: &str, pairs: &[(&str, &str)]) -> Result<TokenResponse, TokenError> {
    let form = {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in pairs {
            form.append_pair(k, v);
        }
        form.finish()
    };
    let response = http
        .post(url)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(reqwest::header::ACCEPT, "application/json")
        .body(form)
        .send()
        .await
        .map_err(|e| TokenError::Other(format!("cannot reach the OpenAI token endpoint: {e}")))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| TokenError::Other(format!("reading the OpenAI token endpoint's reply: {e}")))?;
    if !status.is_success() {
        // The body may echo request details; keep only the OAuth error code.
        let code = serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v| {
            let error = v.get("error")?;
            error.as_str().or_else(|| error.get("code").and_then(|c| c.as_str())).map(str::to_string)
        });
        let message = format!(
            "the OpenAI token endpoint answered HTTP {status}{}",
            code.as_deref().map(|c| format!(" `{c}`")).unwrap_or_default()
        );
        return Err(match code.as_deref() {
            Some(code) if is_dead(code) => TokenError::Dead(message),
            _ => TokenError::Other(message),
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| TokenError::Other(format!("the OpenAI token endpoint's reply is not a token response: {e}")))
}

/// Exchange a checked callback for a grant.
pub async fn exchange(
    http: &reqwest::Client,
    issuer: &Issuer,
    attempt: &Attempt,
    callback: CallbackCode,
    known_client_id: Option<&str>,
) -> Result<Grant, String> {
    let client_id = callback
        .client_id
        .or_else(|| known_client_id.map(str::to_string))
        .ok_or("the sign-in returned no client_id, and this backend has none from an earlier sign-in")?;
    let tokens = post_form(http, &issuer.token_url, &[
        ("grant_type", "authorization_code"),
        ("client_id", &client_id),
        ("code", &callback.code),
        ("code_verifier", &attempt.verifier),
        ("redirect_uri", REDIRECT_URI),
        ("resource", RESOURCE),
    ])
    .await
    .map_err(|e| e.to_string())?;
    let id_token = tokens.id_token.as_deref().ok_or("the token response carried no id_token")?;
    let claims = check_id_token(id_token, &issuer.issuer, &client_id, &attempt.nonce)?;
    if let Some(scope) = &tokens.scope
        && !scope.split_whitespace().any(|s| s == PLAN_SCOPE)
    {
        return Err(format!(
            "the sign-in was not granted `{PLAN_SCOPE}`, so it cannot spend ChatGPT plan usage \
             (granted: {scope}); plan usage needs a ChatGPT Plus or Pro subscription"
        ));
    }
    let refresh_token = tokens.refresh_token.ok_or("the token response carried no refresh_token")?;
    Ok(Grant {
        client_id,
        token_url: issuer.token_url.clone(),
        access_token: tokens.access_token,
        refresh_token,
        expires_at: now() + tokens.expires_in.unwrap_or(3600) as i64,
        account: claims.email.or(claims.sub),
        scope: tokens.scope,
    })
}

/// Refresh `grant`, returning its successor. The refresh token rotates: the
/// caller must store the result before the old one could be sent again.
pub async fn refresh(http: &reqwest::Client, grant: &Grant) -> Result<Grant, TokenError> {
    let tokens = post_form(http, &grant.token_url, &[
        ("grant_type", "refresh_token"),
        ("client_id", &grant.client_id),
        ("refresh_token", &grant.refresh_token),
        ("resource", RESOURCE),
    ])
    .await?;
    Ok(Grant {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token.unwrap_or_else(|| grant.refresh_token.clone()),
        expires_at: now() + tokens.expires_in.unwrap_or(3600) as i64,
        scope: tokens.scope.or_else(|| grant.scope.clone()),
        ..grant.clone()
    })
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    iss: String,
    aud: Audience,
    #[serde(default)]
    nonce: Option<String>,
    exp: i64,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

fn check_id_token(token: &str, issuer: &str, client_id: &str, nonce: &str) -> Result<IdClaims, String> {
    let payload = token.split('.').nth(1).ok_or("the ID token is not a JWT")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|e| format!("the ID token payload is not base64url: {e}"))?;
    let claims: IdClaims =
        serde_json::from_slice(&bytes).map_err(|e| format!("the ID token payload is not the expected JSON: {e}"))?;
    if claims.iss.trim_end_matches('/') != issuer.trim_end_matches('/') {
        return Err(format!("the ID token names issuer {:?}, not {issuer:?}", claims.iss));
    }
    let audience_ok = match &claims.aud {
        Audience::One(a) => a == client_id,
        Audience::Many(v) => v.iter().any(|a| a == client_id),
    };
    if !audience_ok {
        return Err("the ID token was not issued to this client".into());
    }
    if claims.nonce.as_deref() != Some(nonce) {
        return Err("the ID token's nonce does not match this sign-in".into());
    }
    if claims.exp + 60 < now() {
        return Err("the ID token has already expired; check this host's clock".into());
    }
    Ok(claims)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An unsigned JWT carrying `claims`, for a fake token endpoint.
    pub(crate) fn id_token(claims: serde_json::Value) -> String {
        let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!("{}.{}.sig", b64(br#"{"alg":"none"}"#), b64(claims.to_string().as_bytes()))
    }

    /// RFC 7636 Appendix B's verifier and challenge.
    #[test]
    fn the_challenge_matches_rfc_7636() {
        assert_eq!(code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    /// A first sign-in registers through the bootstrap client; a later one
    /// names the issued client and no name hint.
    #[test]
    fn the_authorize_url_registers_once() {
        let issuer = Issuer::openai();
        let attempt = Attempt::fresh();
        let first = url::Url::parse(&authorize_url(&issuer, &attempt, None, "urn:uuid:h")).unwrap();
        let q: std::collections::HashMap<_, _> = first.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], BOOTSTRAP_CLIENT_ID);
        assert_eq!(q["agent_name_hint"], "kaijutsu");
        assert_eq!(q["redirect_uri"], REDIRECT_URI);
        assert_eq!(q["resource"], RESOURCE);
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:h");
        assert_eq!(q["code_challenge"], code_challenge(&attempt.verifier));
        assert!(q["scope"].split(' ').any(|s| s == PLAN_SCOPE));
        let later = authorize_url(&issuer, &attempt, Some("oaiapp_1"), "urn:uuid:h");
        assert!(later.contains("client_id=oaiapp_1") && !later.contains("agent_name_hint"), "{later}");
    }

    /// State is checked before anything else, including a denial; a pasted
    /// full address and a bare query both work.
    #[test]
    fn a_callback_is_checked_state_first() {
        let attempt = Attempt::fresh();
        let s = &attempt.state;
        let good = check_callback(&attempt, &format!("{REDIRECT_URI}?code=c1&state={s}&client_id=oaiapp_1"), None).unwrap();
        assert_eq!(good, CallbackCode { code: "c1".into(), client_id: Some("oaiapp_1".into()) });
        assert_eq!(check_callback(&attempt, &format!("code=c2&state={s}"), None).unwrap().code, "c2");
        let stranger = check_callback(&attempt, "?error=access_denied&state=other", None).unwrap_err();
        assert!(stranger.contains("state does not match"), "{stranger}");
        let denied = check_callback(&attempt, &format!("?error=access_denied&state={s}"), None).unwrap_err();
        assert!(denied.contains("access_denied"), "{denied}");
        let switched = check_callback(&attempt, &format!("?code=c&state={s}&client_id=oaiapp_2"), Some("oaiapp_1")).unwrap_err();
        assert!(switched.contains("different client_id"), "{switched}");
    }

    /// The ID token must name the issuer, this client and this attempt.
    #[test]
    fn id_token_claims_are_checked() {
        let good = serde_json::json!({ "iss": "https://auth.openai.com", "aud": ["oaiapp_1"], "nonce": "n", "exp": now() + 60, "email": "a@b" });
        assert!(check_id_token(&id_token(good.clone()), "https://auth.openai.com/", "oaiapp_1", "n").is_ok());
        assert!(check_id_token(&id_token(good.clone()), "https://auth.openai.com", "oaiapp_1", "other").is_err());
        assert!(check_id_token(&id_token(good), "https://auth.openai.com", "oaiapp_2", "n").is_err());
    }
}

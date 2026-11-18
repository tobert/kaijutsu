//! Signed-in credentials for `chatgpt` backends.
//!
//! A grant is kernel state, not configuration: its refresh token changes on
//! every refresh. Grants live in `<data_dir>/credentials/`, one
//! `<backend>.chatgpt.json` file each, beside `host-id` (this kernel's
//! `ext_agent_host_id`). The directory is kept at mode 0700 and each file is
//! written at 0600, so the tokens stay out of the world-readable `kernel.db`.
//! A looser existing directory is tightened with a warning; a symlinked one
//! is refused.
//!
//! A refresh token rotates, and presenting a spent one can revoke the grant,
//! so within one kernel a refresh token is never sent twice. Two kernels
//! over one data directory would each refresh on their own; run one.
//!
//!
//! - Refreshes for one backend run one at a time, in a task of their own, so
//!   a turn cancelled mid-refresh does not abandon the answer.
//! - Each successor is written (a fresh temporary file synced, then renamed
//!   into place) before its access token is used. If that write fails, the
//!   successor is kept in memory and the kernel uses it until restart.
//! - A refresh that went out and got no usable answer (a timeout, a cut
//!   connection) may have spent its token, so the grant is removed and the
//!   error says to sign in again.
//!
//! A crash between the token endpoint's answer and the write leaves a spent
//! token on disk; the next refresh is refused and the grant is removed, with
//! the same message.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::oauth::{self, Attempt, Issuer, TOKEN_REQUEST_TIMEOUT, TokenError};
use crate::llm::{LlmError, http_user_agent};

/// A grant is refreshed when it has less than this many seconds left.
pub const REFRESH_MARGIN_SECS: i64 = 300;

/// How long a sign-in waits for its pasted callback.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Refuse a backend name that cannot name a credentials file.
pub fn check_backend_name(backend: &str) -> Result<(), String> {
    let safe = !backend.is_empty()
        && !backend.starts_with('.')
        && backend.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if safe {
        Ok(())
    } else {
        Err(format!(
            "a chatgpt backend's name must use letters, digits, '-', '_' or '.' (not leading), \
             because it names its credentials file; {backend:?} does not"
        ))
    }
}

/// One backend's sign-in.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    /// The client id OpenAI issued on the first sign-in.
    pub client_id: String,
    /// The token endpoint the grant was minted at; a refresh dials only this.
    pub token_url: String,
    pub access_token: String,
    pub refresh_token: String,
    /// When `access_token` expires, in seconds since the Unix epoch.
    pub expires_at: i64,
    /// The signed-in account's email (or subject), for status output.
    pub account: Option<String>,
    pub scope: Option<String>,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("client_id", &self.client_id)
            .field("account", &self.account)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// A sign-in waiting for its pasted callback.
struct Pending {
    attempt: Attempt,
    started: Instant,
}

/// The kernel's sign-in store. Cheap to share behind an `Arc`.
pub struct Credentials {
    dir: PathBuf,
    issuer: Issuer,
    http: reqwest::Client,
    token_timeout: Duration,
    /// One lock per backend, held across a refresh, a sign-in's exchange and
    /// a sign-out.
    locks: parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pending: parking_lot::Mutex<HashMap<String, Pending>>,
    /// Refreshed grants whose write failed. They outrank the file.
    unsaved: parking_lot::Mutex<HashMap<String, Grant>>,
    /// Held while the host id is read or created.
    host_id_lock: parking_lot::Mutex<()>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("dir", &self.dir).finish_non_exhaustive()
    }
}

impl Credentials {
    /// A store at `dir` that signs in with OpenAI. Nothing is created until
    /// the first write.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self::with_issuer(dir, Issuer::openai())
    }

    /// A store that signs in at `issuer` (a test server).
    pub fn with_issuer(dir: impl Into<PathBuf>, issuer: Issuer) -> Self {
        Self {
            dir: dir.into(),
            issuer,
            http: reqwest::Client::builder()
                .user_agent(http_user_agent())
                .build()
                .expect("reqwest::Client::builder must succeed on healthy host"),
            token_timeout: TOKEN_REQUEST_TIMEOUT,
            locks: Default::default(),
            pending: Default::default(),
            unsaved: Default::default(),
            host_id_lock: Default::default(),
        }
    }

    /// How long a token request may take (tests shorten it).
    #[cfg(test)]
    fn with_token_timeout(mut self, timeout: Duration) -> Self {
        self.token_timeout = timeout;
        self
    }

    fn grant_path(&self, backend: &str) -> Result<PathBuf, String> {
        check_backend_name(backend)?;
        Ok(self.dir.join(format!("{backend}.chatgpt.json")))
    }

    /// Create the directory at 0700, or tighten an existing one.
    fn secure_dir(&self) -> Result<(), String> {
        match std::fs::symlink_metadata(&self.dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                Err(format!("{} is a symlink; refusing to keep credentials there", self.dir.display()))
            }
            Ok(meta) if !meta.is_dir() => Err(format!("{} is not a directory", self.dir.display())),
            Ok(meta) => {
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
                        .map_err(|e| format!("cannot set {} to mode 0700: {e}", self.dir.display()))?;
                    tracing::warn!(dir = %self.dir.display(), was = format!("{mode:o}"), "credentials directory tightened to 0700");
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.dir)
                .map_err(|e| format!("cannot create {}: {e}", self.dir.display())),
            Err(e) => Err(format!("cannot read {}: {e}", self.dir.display())),
        }
    }

    /// Write `bytes` to `path` at 0600: a new temporary file with a unique
    /// name, created exclusively (so no existing file or link is reused),
    /// synced, renamed into place, and the directory synced. A failed write
    /// removes its temporary file.
    fn write_private(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        self.secure_dir()?;
        let tmp = self.dir.join(format!(".write-{}.tmp", uuid::Uuid::new_v4().simple()));
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)?;
            std::fs::File::open(&self.dir)?.sync_all()
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot write {}: {e}", path.display())
        })
    }

    /// This kernel's `ext_agent_host_id`, created on first use.
    pub fn host_id(&self) -> Result<String, String> {
        let _held = self.host_id_lock.lock();
        self.secure_dir()?;
        let path = self.dir.join("host-id");
        match std::fs::read_to_string(&path) {
            Ok(id) if !id.trim().is_empty() => return Ok(id.trim().to_string()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        }
        let id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        self.write_private(&path, id.as_bytes())?;
        Ok(id)
    }

    /// The current grant for `backend`, if it is signed in.
    pub fn grant(&self, backend: &str) -> Result<Option<Grant>, String> {
        if let Some(grant) = self.unsaved.lock().get(backend) {
            return Ok(Some(grant.clone()));
        }
        let path = self.grant_path(backend)?;
        self.secure_dir()?;
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| format!("{} is not a grant: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    fn put_grant(&self, backend: &str, grant: &Grant) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(grant).expect("a grant serializes");
        self.write_private(&self.grant_path(backend)?, &bytes)?;
        self.unsaved.lock().remove(backend);
        Ok(())
    }

    /// Delete the grant, in memory and on disk. The caller holds the
    /// backend's lock. Returns whether there was one.
    fn drop_grant(&self, backend: &str) -> Result<bool, String> {
        let held = self.unsaved.lock().remove(backend).is_some();
        let path = self.grant_path(backend)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(held),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        }
    }

    /// Sign `backend` out: its grant and any waiting sign-in. A refresh or
    /// sign-in already under way finishes first. Returns whether it was
    /// signed in.
    pub async fn remove(&self, backend: &str) -> Result<bool, String> {
        let lock = self.lock(backend);
        let _held = lock.lock().await;
        self.pending.lock().remove(backend);
        self.drop_grant(backend)
    }

    /// Start a sign-in for `backend` and return the address to open. A new
    /// start replaces one still waiting.
    pub fn begin_sign_in(&self, backend: &str) -> Result<String, String> {
        let known = self.grant(backend)?.map(|g| g.client_id);
        let host_id = self.host_id()?;
        let attempt = Attempt::fresh();
        let url = oauth::authorize_url(&self.issuer, &attempt, known.as_deref(), &host_id);
        self.pending.lock().insert(backend.to_string(), Pending { attempt, started: Instant::now() });
        Ok(url)
    }

    /// Finish `backend`'s waiting sign-in with the address the browser was
    /// sent to, and store the grant.
    pub async fn finish_sign_in(&self, backend: &str, pasted: &str) -> Result<Grant, String> {
        let attempt = {
            let pending = self.pending.lock();
            let p = pending
                .get(backend)
                .ok_or_else(|| format!("no sign-in is waiting for {backend}; run `kj backend login {backend}` first"))?;
            if p.started.elapsed() > SIGN_IN_TIMEOUT {
                return Err(format!(
                    "the sign-in for {backend} started more than {} minutes ago; run `kj backend login {backend}` again",
                    SIGN_IN_TIMEOUT.as_secs() / 60
                ));
            }
            p.attempt.clone()
        };
        let lock = self.lock(backend);
        let _held = lock.lock().await;
        let known = self.grant(backend)?.map(|g| g.client_id);
        let code = oauth::check_callback(&attempt, pasted, known.as_deref())?;
        // The code is single use: a failed exchange needs a new sign-in.
        self.pending.lock().remove(backend);
        let grant = oauth::exchange(&self.http, &self.issuer, &attempt, code, known.as_deref(), self.token_timeout).await?;
        self.put_grant(backend, &grant)?;
        Ok(grant)
    }

    fn lock(&self, backend: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().entry(backend.to_string()).or_default().clone()
    }

    /// A current access token for `backend`, refreshed first when it is
    /// close to expiring. `stale` is a token the server just refused: it is
    /// refreshed unless another request already replaced it.
    ///
    /// The work runs in a task of its own, so dropping this future does not
    /// abandon a refresh between the token endpoint's answer and its write.
    pub async fn bearer(self: &Arc<Self>, backend: &str, stale: Option<&str>) -> Result<String, LlmError> {
        let me = self.clone();
        let (backend, stale) = (backend.to_string(), stale.map(str::to_string));
        tokio::spawn(async move { me.bearer_now(&backend, stale.as_deref()).await })
            .await
            .map_err(|e| LlmError::Unavailable(format!("the token refresh task failed: {e}")))?
    }

    async fn bearer_now(&self, backend: &str, stale: Option<&str>) -> Result<String, LlmError> {
        let lock = self.lock(backend);
        let _held = lock.lock().await;
        let grant = self
            .grant(backend)
            .map_err(LlmError::Unavailable)?
            .ok_or_else(|| LlmError::AuthError(format!("{backend} is not signed in; run `kj backend login {backend}`")))?;
        let refused = stale.is_some_and(|s| s == grant.access_token);
        if !refused && grant.expires_at - now() >= REFRESH_MARGIN_SECS {
            return Ok(grant.access_token);
        }
        match oauth::refresh(&self.http, &grant, self.token_timeout).await {
            Ok(next) => {
                if let Err(e) = self.put_grant(backend, &next) {
                    tracing::error!(
                        backend,
                        error = %e,
                        "a refreshed ChatGPT sign-in could not be saved; it is kept in memory, \
                         and a restart will need `kj backend login`"
                    );
                    self.unsaved.lock().insert(backend.to_string(), next.clone());
                }
                Ok(next.access_token)
            }
            Err(TokenError::Dead(message)) => {
                let _ = self.drop_grant(backend);
                Err(LlmError::AuthError(format!(
                    "{backend}'s sign-in can no longer be refreshed ({message}); run `kj backend login {backend}`"
                )))
            }
            Err(TokenError::Uncertain(message)) => {
                let _ = self.drop_grant(backend);
                Err(LlmError::AuthError(format!(
                    "{backend}'s token refresh got no usable answer ({message}), so its refresh token may be \
                     spent and was not kept; run `kj backend login {backend}`"
                )))
            }
            Err(TokenError::Refused(message)) => Err(LlmError::AuthError(message)),
            Err(TokenError::Unspent(message)) => Err(LlmError::NetworkError(message)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_council::test_server::{Reply, reply, serve};

    fn issuer(base: &str) -> Issuer {
        Issuer { authorize_url: format!("{base}/authorize"), token_url: format!("{base}/token"), issuer: "https://auth.openai.com".into() }
    }

    fn grant(token_url: &str, expires_in: i64) -> Grant {
        Grant {
            client_id: "oaiapp_1".into(),
            token_url: token_url.into(),
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            expires_at: now() + expires_in,
            account: None,
            scope: None,
        }
    }

    /// A whole sign-in: the pasted callback is exchanged with the attempt's
    /// verifier, the issued client id is kept, and the grant lands at 0600 in
    /// a 0700 directory. A second sign-in names the issued client.
    #[tokio::test]
    async fn a_pasted_callback_signs_in() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("credentials");
        let (base, seen) = serve(vec![]).await;
        let creds = Credentials::with_issuer(&dir, issuer(&base));
        let url = creds.begin_sign_in("gpt").unwrap();
        let q: HashMap<_, _> = url::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "dynamic_agent_client");

        let id_token = oauth::tests::id_token(serde_json::json!({
            "iss": "https://auth.openai.com", "aud": "oaiapp_1", "nonce": q["nonce"], "exp": now() + 600, "email": "amy@example",
        }));
        let (base2, seen2) = serve(vec![reply(200, serde_json::json!({
            "access_token": "a1", "refresh_token": "r1", "expires_in": 3600, "id_token": id_token,
            "scope": "openid offline_access chatgpt.tokens.use.direct",
        }).to_string())]).await;
        let creds = Credentials { issuer: issuer(&base2), ..creds };
        let pasted = format!("{}?code=CODE&state={}&client_id=oaiapp_1", oauth::REDIRECT_URI, q["state"]);
        let grant = creds.finish_sign_in("gpt", &pasted).await.unwrap();
        assert_eq!((grant.client_id.as_str(), grant.account.as_deref()), ("oaiapp_1", Some("amy@example")));
        assert!(seen.lock().unwrap().is_empty());

        let body = seen2.lock().unwrap()[0].body.clone();
        let form: HashMap<_, _> = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(oauth::code_challenge(&form["code_verifier"]), q["code_challenge"]);
        assert_eq!(form["redirect_uri"], oauth::REDIRECT_URI);

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("gpt.chatgpt.json")), 0o600);
        assert_eq!(creds.grant("gpt").unwrap().unwrap(), grant);
        assert!(creds.finish_sign_in("gpt", &pasted).await.is_err(), "a code is single use");
        assert!(creds.begin_sign_in("gpt").unwrap().contains("client_id=oaiapp_1"));
    }

    /// A token close to expiry is refreshed once, and the rotated pair is
    /// stored before use; concurrent callers share that refresh.
    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, seen) = serve(vec![reply(200, r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#)]).await;
        let creds = Arc::new(Credentials::with_issuer(tmp.path(), issuer(&base)));
        creds.put_grant("gpt", &grant(&format!("{base}/token"), 10)).unwrap();
        let calls: Vec<_> = (0..4).map(|_| {
            let creds = creds.clone();
            tokio::spawn(async move { creds.bearer("gpt", None).await.unwrap() })
        }).collect();
        for call in calls {
            assert_eq!(call.await.unwrap(), "new-access");
        }
        assert_eq!(seen.lock().unwrap().len(), 1);
        let form = seen.lock().unwrap()[0].body.clone();
        assert!(form.contains("grant_type=refresh_token") && form.contains("refresh_token=old-refresh"), "{form}");
        assert_eq!(creds.grant("gpt").unwrap().unwrap().refresh_token, "new-refresh");
    }

    /// A refused token is refreshed even before it expires, unless another
    /// caller already replaced it.
    #[tokio::test]
    async fn a_refused_token_is_refreshed_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, seen) = serve(vec![reply(200, r#"{"access_token":"new-access","expires_in":3600}"#)]).await;
        let creds = Arc::new(Credentials::with_issuer(tmp.path(), issuer(&base)));
        creds.put_grant("gpt", &grant(&format!("{base}/token"), 3000)).unwrap();
        assert_eq!(creds.bearer("gpt", None).await.unwrap(), "old-access");
        assert_eq!(creds.bearer("gpt", Some("old-access")).await.unwrap(), "new-access");
        assert_eq!(creds.bearer("gpt", Some("old-access")).await.unwrap(), "new-access");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(creds.grant("gpt").unwrap().unwrap().refresh_token, "old-refresh", "an unrotated refresh keeps its token");
    }

    /// A dead grant is removed and the error says to sign in again; no
    /// sign-in sends nothing.
    #[tokio::test]
    async fn a_dead_grant_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, seen) = serve(vec![reply(400, r#"{"error":"refresh_token_reused"}"#)]).await;
        let creds = Arc::new(Credentials::with_issuer(tmp.path(), issuer(&base)));
        let err = creds.bearer("gpt", None).await.unwrap_err();
        assert!(matches!(&err, LlmError::AuthError(m) if m.contains("kj backend login gpt")), "{err:?}");
        assert!(seen.lock().unwrap().is_empty());
        creds.put_grant("gpt", &grant(&format!("{base}/token"), 0)).unwrap();
        let err = creds.bearer("gpt", None).await.unwrap_err();
        assert!(matches!(&err, LlmError::AuthError(m) if m.contains("refresh_token_reused")), "{err:?}");
        assert!(creds.grant("gpt").unwrap().is_none());
    }

    /// A refresh that went out and got no answer in time may have spent its
    /// token: the grant is dropped rather than sent again, and the error says
    /// to sign in. A request that never connected keeps the grant.
    ///
    /// Falsified by a second token request after the timeout, or by a grant
    /// lost when the endpoint was unreachable.
    #[tokio::test]
    async fn an_unanswered_refresh_is_not_sent_again() {
        let tmp = tempfile::tempdir().unwrap();
        let slow = Reply { delay: Duration::from_secs(5), ..reply(200, r#"{"access_token":"late","expires_in":3600}"#) };
        let (base, seen) = serve(vec![slow]).await;
        let creds = Arc::new(Credentials::with_issuer(tmp.path(), issuer(&base)).with_token_timeout(Duration::from_millis(200)));
        creds.put_grant("gpt", &grant(&format!("{base}/token"), 10)).unwrap();
        let err = creds.bearer("gpt", None).await.unwrap_err();
        assert!(matches!(&err, LlmError::AuthError(m) if m.contains("may be spent")), "{err:?}");
        assert!(creds.grant("gpt").unwrap().is_none());
        assert!(creds.bearer("gpt", None).await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 1);

        let unreachable = "http://127.0.0.1:9/token";
        creds.put_grant("gpt", &grant(unreachable, 10)).unwrap();
        let err = creds.bearer("gpt", None).await.unwrap_err();
        assert!(matches!(err, LlmError::NetworkError(_)), "{err:?}");
        assert!(creds.grant("gpt").unwrap().is_some(), "an unsent refresh keeps the grant");
    }

    /// A 4xx refusal that does not end the grant is permanent, not a
    /// network error to retry, and the grant stays.
    #[tokio::test]
    async fn a_refused_refresh_is_permanent() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, _) = serve(vec![reply(400, r#"{"error":"invalid_request"}"#)]).await;
        let creds = Arc::new(Credentials::with_issuer(tmp.path(), issuer(&base)));
        creds.put_grant("gpt", &grant(&format!("{base}/token"), 10)).unwrap();
        let err = creds.bearer("gpt", None).await.unwrap_err();
        assert!(matches!(&err, LlmError::AuthError(m) if m.contains("invalid_request")), "{err:?}");
        assert!(creds.grant("gpt").unwrap().is_some());
    }

    /// A rate-limited refresh is retryable and keeps the grant: many
    /// contexts refreshing at once must not each be told to sign in again.
    #[tokio::test]
    async fn a_rate_limited_refresh_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, _) = serve(vec![reply(429, r#"{"error":"rate_limited"}"#)]).await;
        let creds = Arc::new(Credentials::with_issuer(tmp.path(), issuer(&base)));
        creds.put_grant("gpt", &grant(&format!("{base}/token"), 10)).unwrap();
        let err = creds.bearer("gpt", None).await.unwrap_err();
        assert!(matches!(err, LlmError::NetworkError(_)), "{err:?}");
        assert!(creds.grant("gpt").unwrap().is_some());
    }

    /// A caller dropped mid-refresh does not abandon the refresh: the
    /// rotated pair still reaches disk.
    ///
    /// Falsified by the old refresh token on disk after the answer arrived.
    #[tokio::test]
    async fn a_cancelled_caller_does_not_lose_the_rotated_token() {
        let tmp = tempfile::tempdir().unwrap();
        let slow = Reply { delay: Duration::from_millis(300), ..reply(200, r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#) };
        let (base, _) = serve(vec![slow]).await;
        let creds = Arc::new(Credentials::with_issuer(tmp.path(), issuer(&base)));
        creds.put_grant("gpt", &grant(&format!("{base}/token"), 10)).unwrap();
        let caller = { let creds = creds.clone(); tokio::spawn(async move { creds.bearer("gpt", None).await }) };
        tokio::time::sleep(Duration::from_millis(100)).await;
        caller.abort();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(creds.grant("gpt").unwrap().unwrap().refresh_token, "new-refresh");
    }

    /// A rotated grant whose write fails is kept in memory and used, so the
    /// spent refresh token on disk is never sent again.
    #[tokio::test]
    async fn an_unsaved_refresh_is_kept_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, seen) = serve(vec![reply(200, r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#)]).await;
        let creds = Arc::new(Credentials::with_issuer(tmp.path(), issuer(&base)));
        creds.put_grant("gpt", &grant(&format!("{base}/token"), 10)).unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let first = creds.bearer("gpt", None).await;
        let second = creds.bearer("gpt", None).await;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!((first.unwrap(), second.unwrap()), ("new-access".to_string(), "new-access".to_string()));
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(creds.grant("gpt").unwrap().unwrap().refresh_token, "new-refresh");
    }

    /// Sign-out waits for a refresh under way, so the refresh cannot write
    /// the grant back after it.
    #[tokio::test]
    async fn sign_out_is_not_undone_by_a_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let slow = Reply { delay: Duration::from_millis(300), ..reply(200, r#"{"access_token":"new-access","refresh_token":"r1","expires_in":3600}"#) };
        let (base, _) = serve(vec![slow]).await;
        let creds = Arc::new(Credentials::with_issuer(tmp.path(), issuer(&base)));
        creds.put_grant("gpt", &grant(&format!("{base}/token"), 10)).unwrap();
        let refresh = { let creds = creds.clone(); tokio::spawn(async move { creds.bearer("gpt", None).await }) };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(creds.remove("gpt").await.unwrap());
        refresh.await.unwrap().unwrap();
        assert!(creds.grant("gpt").unwrap().is_none());
    }

    /// A write replaces a grant file that had looser permissions and leaves
    /// no temporary file behind.
    #[test]
    fn a_write_leaves_a_private_file_and_no_temporary() {
        let tmp = tempfile::tempdir().unwrap();
        let creds = Credentials::at(tmp.path());
        let path = tmp.path().join("gpt.chatgpt.json");
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        creds.put_grant("gpt", &grant("http://t", 10)).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("gpt.chatgpt.json")]);
    }

    /// A looser existing directory is tightened; a symlinked one is refused;
    /// a backend name cannot leave the directory.
    #[test]
    fn the_directory_is_kept_private() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("credentials");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let creds = Credentials::at(&dir);
        let id = creds.host_id().unwrap();
        assert!(id.starts_with("urn:uuid:"), "{id}");
        assert_eq!(creds.host_id().unwrap(), id);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(Credentials::at(&link).host_id().unwrap_err().contains("symlink"));
        assert!(creds.grant("../x").is_err());
    }
}

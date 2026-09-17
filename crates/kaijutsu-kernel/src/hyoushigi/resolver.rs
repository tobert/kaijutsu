//! CAS preparation on the existing runtime, outside timeline locks.

use std::sync::Arc;
use std::time::Duration;

use kaijutsu_audio::{Clip, CLIP_MIME};
use kaijutsu_cas::{ContentHash, ContentStore, FileStore};
use kaijutsu_hyoushigi::{ContextHash, ResolveError, Resolution, Resolver, ResolverCtx, ResolverId};

use super::{ABC_MIME, validate_abc};

/// Prepare an immutable CAS artifact for commitment. Its hash is the basis;
/// model-context freshness belongs to the producer that chose the artifact.
/// Source bytes remain source bytes; rendering happens at the write barrier.
pub(super) struct CasCommitResolver {
    cas: Arc<FileStore>,
}

impl CasCommitResolver {
    pub(super) fn new(cas: Arc<FileStore>) -> Self { Self { cas } }
    /// The single resolver id; recipes name it.
    pub const ID: &'static str = "cas_commit";

    fn param_str<'a>(params: &'a serde_json::Value, key: &str) -> Result<&'a str, ResolveError> {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| ResolveError::Failed(format!("cas_commit: missing `{key}` param")))
    }

    fn hash_param(params: &serde_json::Value) -> Result<ContentHash, ResolveError> {
        let s = Self::param_str(params, "hash")?;
        ContentHash::from_str_checked(s)
            .map_err(|e| ResolveError::Failed(format!("cas_commit: malformed hash: {e}")))
    }
}

impl Resolver for CasCommitResolver {
    fn id(&self) -> ResolverId {
        ResolverId::new(Self::ID)
    }

    fn estimate_cost(&self, _params: &serde_json::Value, _rctx: &dyn ResolverCtx) -> Duration {
        // Initial estimate, including no measured queue-time model. Readiness
        // and deadline disposition expose misses instead of promising this cost.
        Duration::from_millis(20)
    }

    fn compute_basis(&self, params: &serde_json::Value, _rctx: &dyn ResolverCtx) -> ContextHash {
        let input = match (Self::hash_param(params), Self::param_str(params, "mime")) {
            (Ok(hash), Ok(mime)) => serde_json::json!({"hash": hash.as_str(), "mime": mime}),
            _ => serde_json::json!({"invalid": params}),
        };
        ContextHash::of(&serde_json::to_vec(&input).expect("recipe input is JSON"))
    }

    fn resolve(
        &self,
        params: &serde_json::Value,
        _rctx: &dyn ResolverCtx,
    ) -> kaijutsu_hyoushigi::ResolveFuture {
        let cas = self.cas.clone();
        let params = params.clone();
        prepare_output(move || Self::prepare(&cas, &params))
    }
}

impl CasCommitResolver {
    fn prepare(cas: &FileStore, params: &serde_json::Value) -> Result<Resolution, ResolveError> {
        let hash = Self::hash_param(params)?;
        let mime = Self::param_str(params, "mime")?.to_string();
        let bytes = cas.retrieve(&hash)
            .map_err(|e| ResolveError::Failed(format!("cas_commit: CAS read: {e}")))?
            .ok_or_else(|| ResolveError::Failed(format!("cas_commit: hash not in CAS: {}", hash.as_str())))?;
        if ContentHash::from_data(&bytes) != hash {
            return Err(ResolveError::Failed(format!("cas_commit: hash mismatch for {}", hash.as_str())));
        }
        if mime.as_str() == ABC_MIME {
            validate_abc(&bytes).map_err(|e| ResolveError::Failed(format!("cas_commit: {e}")))?;
        } else if mime.as_str() == CLIP_MIME {
            let json = std::str::from_utf8(&bytes)
                .map_err(|e| ResolveError::Failed(format!("cas_commit: clip record is not UTF-8: {e}")))?;
            Clip::parse(json).map_err(|e| ResolveError::Failed(format!("cas_commit: {e}")))?;
        }
        Ok(Resolution::new(bytes, mime))
    }

}


/// Share the bounded blocking preparation budget with admitted model output.
pub(super) fn prepare_output(
    work: impl FnOnce() -> Result<Resolution, ResolveError> + Send + 'static,
) -> kaijutsu_hyoushigi::ResolveFuture {
    static READS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let slots = READS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4))).clone();
    prepare_on_runtime(slots, work)
}

fn prepare_on_runtime(
    slots: Arc<tokio::sync::Semaphore>,
    work: impl FnOnce() -> Result<Resolution, ResolveError> + Send + 'static,
) -> kaijutsu_hyoushigi::ResolveFuture {
    Box::pin(async move {
        // A cancelled blocking operation may already be running. Its permit
        // stays with the closure until it finishes, preserving the shared bound.
        let permit = slots.acquire_owned().await
            .map_err(|e| ResolveError::Failed(format!("CAS preparation admission: {e}")))?;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        });
        tokio_util::task::AbortOnDropHandle::new(task).await
            .map_err(|e| ResolveError::Failed(format!("CAS preparation task: {e}")))?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_bytes_cannot_satisfy_the_requested_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let cas = FileStore::at_path(dir.path());
        let hash = cas.store(b"accepted bytes", "text/plain").unwrap();
        std::fs::write(cas.path(&hash).unwrap(), b"different bytes").unwrap();
        let result = CasCommitResolver::prepare(&cas, &serde_json::json!({"hash": hash.as_str(), "mime": "text/plain"}));
        assert!(matches!(result, Err(ResolveError::Failed(message)) if message.contains("hash mismatch")));
    }

    #[test]
    fn basis_uses_canonical_hash_and_declared_interpretation() {
        struct Context;
        impl ResolverCtx for Context {
            fn now(&self) -> kaijutsu_types::Tick { kaijutsu_types::Tick::ZERO }
            fn ambient(&self, _: &str) -> Option<Vec<u8>> { None }
            fn content_before(&self, _: kaijutsu_types::Tick) -> Option<kaijutsu_hyoushigi::ContentRef> { None }
        }
        let dir = tempfile::tempdir().unwrap();
        let resolver = CasCommitResolver::new(Arc::new(FileStore::at_path(dir.path())));
        let hash = ContentHash::from_data(b"data");
        let basis = |hash: &str, mime: &str| resolver.compute_basis(&serde_json::json!({"hash": hash, "mime": mime}), &Context);
        assert_eq!(basis(hash.as_str(), "text/plain"), basis(&hash.as_str().to_uppercase(), "text/plain"));
        assert_ne!(basis(hash.as_str(), "text/plain"), basis(hash.as_str(), "application/json"));
        assert_ne!(resolver.compute_basis(&serde_json::json!({}), &Context), resolver.compute_basis(&serde_json::json!({"mime": "text/plain"}), &Context));
    }

    #[tokio::test]
    async fn preparation_errors_reach_the_owner_and_release_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(FileStore::at_path(dir.path()));
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let hash = ContentHash::from_data(b"absent");
        for (params, expected) in [
            (serde_json::json!({"mime": "text/plain"}), "missing `hash`"),
            (serde_json::json!({"hash": hash.as_str()}), "missing `mime`"),
            (serde_json::json!({"hash": "invalid", "mime": "text/plain"}), "malformed hash"),
            (serde_json::json!({"hash": hash.as_str(), "mime": "text/plain"}), "hash not in CAS"),
        ] {
            let store = cas.clone();
            let result = prepare_on_runtime(slots.clone(), move || CasCommitResolver::prepare(&store, &params)).await;
            assert!(matches!(result, Err(ResolveError::Failed(message)) if message.contains(expected)));
            assert_eq!(slots.available_permits(), 1);
        }
    }

    #[tokio::test]
    async fn cancelled_running_preparation_retains_admission_until_it_finishes() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let (finish, finish_rx) = std::sync::mpsc::channel();
        let owner = tokio::spawn(prepare_on_runtime(slots.clone(), move || {
            let _ = started.send(());
            finish_rx.recv().expect("controlled finish");
            Ok(Resolution::new(b"discarded", "text/plain"))
        }));
        tokio::time::timeout(Duration::from_secs(5), started_rx).await.unwrap().unwrap();
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        assert_eq!(slots.available_permits(), 0, "a running operation retains its slot after owner cancellation");
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = ran.clone();
        let mut queued = prepare_on_runtime(slots.clone(), move || {
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(Resolution::new(b"queued", "text/plain"))
        });
        assert!(futures::poll!(queued.as_mut()).is_pending());
        drop(queued);
        finish.send(()).unwrap();
        let released = tokio::time::timeout(Duration::from_secs(5), slots.acquire_owned()).await.unwrap().unwrap();
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "cancelled admission must never launch work");
        drop(released);
    }
}

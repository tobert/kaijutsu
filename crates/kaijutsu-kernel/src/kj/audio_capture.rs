//! Bounded retained-take publication jobs. Hardware remains owned by peers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use parking_lot::Mutex;
use serde_json::{Value, json};
use uuid::Uuid;
use crate::{Kernel, peers::InvokeRequest, vfs::VfsOps};
use super::{KjDispatcher, KjResult};

const MAX_ACTIVE: usize = 8;
const MAX_JOBS: usize = 32;
const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct KeepJobs(Mutex<BTreeMap<Uuid, Arc<Job>>>);

struct Job {
    id: Uuid,
    node: String,
    instance: String,
    path: Mutex<PathBuf>,
    retired_paths: Mutex<Vec<PathBuf>>,
    state: Mutex<State>,
    operation: tokio::sync::Mutex<()>,
}

struct State {
    phase: &'static str,
    bytes: u64,
    hash: Option<String>,
    error: Option<String>,
}

fn terminal(phase: &str) -> bool { matches!(phase, "complete" | "cancelled") }

impl Job {
    fn view(&self) -> Value {
        let state = self.state.lock();
        json!({"id": self.id, "node": self.node, "instance": self.instance,
            "state": state.phase, "bytes": state.bytes, "hash": state.hash,
            "error": state.error, "protection": if state.hash.is_some() { "kernel-cas" } else if state.phase == "reserving" { "unconfirmed" } else if state.phase == "cancelled" { "none" } else { "daemon-ram" }})
    }

    fn error(&self, error: String) { self.state.lock().error = Some(error); }
}

impl KeepJobs {
    fn insert(&self, job: Arc<Job>) -> Result<(), String> {
        let mut jobs = self.0.lock();
        if jobs.values().filter(|j| !terminal(j.state.lock().phase)).count() >= MAX_ACTIVE {
            return Err(format!("audio keep has {MAX_ACTIVE} active jobs; finish or cancel one first"));
        }
        if jobs.len() >= MAX_JOBS {
            let old = jobs.iter().find(|(_, j)| terminal(j.state.lock().phase)).map(|(id, _)| *id)
                .ok_or_else(|| "audio keep job limit reached".to_string())?;
            jobs.remove(&old);
        }
        jobs.insert(job.id, job);
        Ok(())
    }

    fn get(&self, id: Uuid) -> Result<Arc<Job>, String> {
        self.0.lock().get(&id).cloned().ok_or_else(|| format!("unknown audio keep {id}; jobs are ephemeral"))
    }
}

async fn select_instance(kernel: &Kernel, node: &str) -> Result<String, String> {
    let peers: Vec<_> = kernel.list_peers().await.into_iter().filter(|p| p.nick == node).collect();
    if peers.len() != 1 { return Err(format!("'{node}' has {} connected instances; exactly one is required", peers.len())); }
    Ok(crate::peers::peer_key(&peers[0].nick, &peers[0].instance))
}

async fn invoke(kernel: &Kernel, instance: &str, action: &str, params: Value) -> Result<Value, String> {
    let sender = kernel.peers().read().get_invoke_sender_by_instance(instance)
        .ok_or_else(|| format!("audio instance '{instance}' is disconnected; protected material has not been released"))?;
    let (reply, receiver) = tokio::sync::oneshot::channel();
    let request = InvokeRequest { action: action.into(), params: serde_json::to_vec(&params).map_err(|e| e.to_string())?, reply };
    let response = tokio::time::timeout(kaijutsu_types::timeout::peer::KERNEL_WAIT, async {
        sender.send(request).await.map_err(|_| "audio peer disconnected".to_string())?;
        receiver.await.map_err(|_| "audio peer dropped reply".to_string())?.result.map_err(|e| e.to_string())
    }).await.map_err(|_| "audio peer timed out; keep acknowledgement may be pending".to_string())??;
    serde_json::from_slice(&response).map_err(|e| format!("invalid audio peer reply: {e}"))
}

fn validate_reply(value: &Value, id: Uuid) -> Result<u64, String> {
    if value.get("id").and_then(Value::as_str) != Some(id.to_string().as_str()) {
        return Err("audio peer replied with a different keep id".into());
    }
    let bytes = value.get("bytes").and_then(Value::as_u64).ok_or("audio keep reply has no byte count")?;
    if bytes > MAX_BYTES { return Err(format!("audio keep exceeds {MAX_BYTES} encoded bytes")); }
    Ok(bytes)
}

fn reported_path(job: &Job, status: &Value) -> Result<PathBuf, String> {
    let path = PathBuf::from(status["upload_path"].as_str().ok_or("audio keep status has no upload path")?);
    if *job.path.lock() != path && !job.retired_paths.lock().contains(&path) {
        return Err("audio keep status names an upload path not owned by this job".into());
    }
    Ok(path)
}

fn result(value: Value) -> KjResult { KjResult::ok_with_data(value.to_string(), value) }

async fn new_staging(kernel: &Kernel) -> Result<PathBuf, String> {
    let mapped = kernel.vfs().real_path(Path::new("/tmp")).await.map_err(|e| e.to_string())?;
    if mapped.as_deref() != Some(Path::new("/tmp")) { return Err("audio keep requires VFS /tmp mapped to host /tmp".into()); }
    let dir = PathBuf::from(format!("/tmp/kaijutsu-audio-{}", Uuid::new_v4()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)] { use std::os::unix::fs::DirBuilderExt; builder.mode(0o700); }
    builder.create(&dir).map_err(|e| format!("cannot create audio upload staging: {e}"))?;
    let path = dir.join("capture.json");
    match kernel.vfs().real_path(&dir).await {
        Ok(Some(mapped)) if mapped == dir && kernel.vfs().resolve_real_path_sync(&path).as_ref() == Some(&path) => Ok(path),
        other => {
            std::fs::remove_dir(&dir).map_err(|e| format!("staging mapping failed and cleanup failed: {e}"))?;
            Err(format!("audio upload staging does not map to its host path: {other:?}"))
        }
    }
}

impl KjDispatcher {
    pub(super) async fn audio_devices(&self, node: String) -> KjResult {
        let queried = async {
            let instance = select_instance(self.kernel(), &node).await?;
            invoke(self.kernel(), &instance, "inventory", json!({})).await
        }.await;
        match queried { Ok(value) => result(value), Err(e) => KjResult::Err(e) }
    }

    pub(super) fn audio_keep_status(&self, id: Uuid) -> KjResult {
        match self.audio_keeps.get(id) { Ok(job) => result(job.view()), Err(e) => KjResult::Err(e) }
    }

    pub(super) async fn audio_keep(&self, node: String, source: String, generation: Uuid, seconds: u32) -> KjResult {
        let prepared = async {
            let instance = select_instance(self.kernel(), &node).await?;
            let id = Uuid::new_v4();
            let path = new_staging(self.kernel()).await?;
            let job = Arc::new(Job { id, node, instance, path: Mutex::new(path), retired_paths: Mutex::new(Vec::new()), state: Mutex::new(State {
                phase: "reserving", bytes: 0, hash: None, error: None,
            }), operation: tokio::sync::Mutex::new(()) });
            if let Err(e) = self.audio_keeps.insert(job.clone()) { cleanup(&job.path.lock())?; return Err(e); }
            Ok(job)
        }.await;
        let job = match prepared { Ok(job) => job, Err(e) => return KjResult::Err(e) };
        let params = json!({"id":job.id,"source":source,"generation":generation,"seconds":seconds,"upload_path":*job.path.lock()});
        let acknowledgement = invoke(self.kernel(), &job.instance, "keep", params).await;
        match acknowledgement.and_then(|value| {
            let bytes = validate_reply(&value, job.id)?;
            if value["state"] != "uploading" { return Err(format!("keep was not acknowledged: {value}")); }
            Ok(bytes)
        }) {
            Ok(bytes) => { let mut state = job.state.lock(); state.phase = "uploading"; state.bytes = bytes; }
            Err(e) => job.error(e),
        }
        let kernel = self.kernel().clone();
        let worker = job.clone();
        tokio::spawn(async move { run_job(kernel, worker).await; });
        if job.state.lock().phase == "reserving" {
            return KjResult::Err(format!("keep {} has no confirmed protection acknowledgement; inspect with kj audio keep-status {}: {}", job.id, job.id, job.state.lock().error.as_deref().unwrap_or("unknown reply")));
        }
        result(job.view())
    }

    pub(super) async fn audio_keep_retry(&self, id: Uuid) -> KjResult {
        let job = match self.audio_keeps.get(id) { Ok(job) => job, Err(e) => return KjResult::Err(e) };
        let _operation = job.operation.lock().await;
        if job.state.lock().phase != "failed" { return KjResult::Err("only a failed upload can be retried; inspect keep-status first".into()); }
        if let Err(e) = cleanup_job(&job) { return KjResult::Err(e); }
        job.retired_paths.lock().clear();
        let prepared = new_staging(self.kernel()).await;
        let path = match prepared { Ok(path) => path, Err(e) => return KjResult::Err(e) };
        // Remember the target before invoking: the upload may start even when
        // its acknowledgement is lost. Never delete a possibly live target.
        let previous = std::mem::replace(&mut *job.path.lock(), path.clone());
        job.retired_paths.lock().push(previous);
        job.state.lock().phase = "reserving";
        match invoke(self.kernel(), &job.instance, "keep_retry", json!({"id":id,"upload_path":path})).await {
            Ok(value) => match validate_reply(&value, id) {
                Ok(bytes) if value["state"] == "uploading" => { let mut state = job.state.lock(); state.phase = "uploading"; state.bytes = bytes; state.error = None; }
                Ok(_) => job.error("daemon did not acknowledge retry".into()),
                Err(e) => job.error(e),
            },
            Err(e) => job.error(e),
        }
        result(job.view())
    }

    pub(super) async fn audio_keep_cancel(&self, id: Uuid) -> KjResult {
        let job = match self.audio_keeps.get(id) { Ok(job) => job, Err(e) => return KjResult::Err(e) };
        let _operation = job.operation.lock().await;
        if terminal(job.state.lock().phase) { return result(job.view()); }
        match invoke(self.kernel(), &job.instance, "keep_cancel", json!({"id":id})).await {
            Ok(value) if value["id"].as_str() == Some(id.to_string().as_str()) => {
                if value["state"] == "cancelling" { job.state.lock().phase = "cancelling"; return result(job.view()); }
                if value["state"] != "cancelled" { job.error("daemon did not acknowledge cancellation".into()); return result(job.view()); }
                if let Err(e) = cleanup_job(&job) { job.error(e); return result(job.view()); }
                let mut state = job.state.lock(); state.phase = "cancelled"; state.error = None;
            }
            Ok(_) => job.error("audio cancel replied with a different keep id".into()),
            Err(e) => job.error(e),
        }
        result(job.view())
    }
}

fn cleanup(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.to_string()) }
    match std::fs::remove_dir(path.parent().ok_or("audio staging has no parent")?) {
        Ok(()) => Ok(()), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), Err(e) => Err(e.to_string()),
    }
}

fn cleanup_job(job: &Job) -> Result<(), String> {
    cleanup(&job.path.lock())?;
    for path in job.retired_paths.lock().iter() { cleanup(path)?; }
    Ok(())
}

async fn run_job(kernel: Arc<Kernel>, job: Arc<Job>) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _operation = job.operation.lock().await;
        if terminal(job.state.lock().phase) { return; }
        if let Err(e) = advance(&kernel, &job).await { job.error(e); }
    }
}

async fn advance(kernel: &Arc<Kernel>, job: &Arc<Job>) -> Result<(), String> {
    if terminal(job.state.lock().phase) { return Ok(()); }
    if job.state.lock().hash.is_none() {
        let status = invoke(kernel, &job.instance, "keep_status", json!({"id":job.id})).await?;
        let bytes = validate_reply(&status, job.id)?;
        let upload_path = reported_path(job, &status)?;
        match status["state"].as_str() {
            Some("cancelling") => { job.state.lock().phase = "cancelling"; return Ok(()); }
            Some("cancelled") => { cleanup_job(job)?; let mut state = job.state.lock(); state.phase = "cancelled"; state.error = None; return Ok(()); }
            Some("preparing") => { job.state.lock().phase = "reserving"; return Ok(()); }
            Some("uploading") => { let mut state = job.state.lock(); state.phase = "uploading"; state.bytes = bytes; state.error = None; return Ok(()); }
            Some("failed") => { job.state.lock().phase = "failed"; return Err(status["error"].as_str().unwrap_or("daemon upload failed; material remains protected").into()); }
            Some("uploaded") => {},
            _ => return Err(format!("unknown audio keep state: {status}")),
        }
        let expected = status["hash"].as_str().ok_or("uploaded take has no hash")?.to_string();
        kaijutsu_cas::ContentHash::from_str_checked(&expected).map_err(|e| e.to_string())?;
        let cas = kernel.cas().clone();
        let path = upload_path;
        job.state.lock().phase = "accepting";
        let hash = tokio::task::spawn_blocking(move || accept_file(&cas, &path, bytes, &expected)).await.map_err(|e| e.to_string())??;
        let mut state = job.state.lock(); state.hash = Some(hash); state.bytes = bytes; state.phase = "releasing"; state.error = None;
    }
    let hash = job.state.lock().hash.clone().expect("accepted hash");
    let reply = invoke(kernel, &job.instance, "keep_release", json!({"id":job.id,"hash":hash})).await?;
    if reply["id"].as_str() != Some(job.id.to_string().as_str()) { return Err("audio release replied with a different keep id".into()); }
    if reply["state"] != "released" { return Err("daemon did not acknowledge release".into()); }
    cleanup_job(job)?;
    let mut state = job.state.lock(); state.phase = "complete"; state.error = None;
    Ok(())
}

fn accept_file(cas: &kaijutsu_cas::FileStore, path: &Path, bytes: u64, expected: &str) -> Result<String, String> {
    use std::io::Read;
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.file_type().is_file() || metadata.len() != bytes || bytes > MAX_BYTES { return Err("audio upload is not a regular file with the reported bounded length".into()); }
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut writer = cas.create_streaming_writer("application/vnd.kaijutsu.midi-history+json").map_err(|e| e.to_string())?;
    let mut buffer = [0u8; 65536];
    let mut count = 0u64;
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 { break; }
        count += n as u64;
        if count > bytes { return Err("audio upload changed length during acceptance".into()); }
        writer.write(&buffer[..n]).map_err(|e| e.to_string())?;
    }
    if count != bytes { return Err("audio upload is incomplete".into()); }
    let sealed = writer.finalize().map_err(|e| e.to_string())?;
    if sealed.content_hash.as_str() != expected { return Err("audio upload hash mismatch; protected take was not released".into()); }
    Ok(sealed.content_hash.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn instance_selection_rejects_ambiguous_nodes() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        for instance in ["one", "two"] {
            d.kernel().attach_peer(crate::peers::PeerConfig { nick:"audio/test".into(), instance:instance.into(), principal:None }, None).await.unwrap();
        }
        assert!(select_instance(d.kernel(), "audio/test").await.unwrap_err().contains("2 connected instances"));
    }

    #[tokio::test]
    async fn old_job_never_invokes_replacement_instance() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        d.kernel().attach_peer(crate::peers::PeerConfig { nick:"audio/test".into(), instance:"replacement".into(), principal:None }, Some(sender)).await.unwrap();
        assert!(invoke(d.kernel(), "original", "keep_release", json!({})).await.is_err());
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn staging_requires_real_tmp_and_uses_private_unique_directories() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        assert!(new_staging(d.kernel()).await.is_err());
        d.kernel().mount("/tmp", crate::vfs::LocalBackend::new("/tmp")).await;
        let first = new_staging(d.kernel()).await.unwrap();
        let second = new_staging(d.kernel()).await.unwrap();
        assert_ne!(first, second);
        assert!(!first.exists(), "uploader exclusively creates its file");
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(first.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        cleanup(&first).unwrap();
        cleanup(&second).unwrap();
    }

    #[tokio::test]
    async fn publication_precedes_release_and_repeated_step_is_safe() {
        use kaijutsu_cas::ContentStore;
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("owned");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("capture.json");
        std::fs::write(&path, b"take").unwrap();
        let hash = kaijutsu_cas::ContentHash::from_data(b"take");
        let job = Arc::new(Job { id:Uuid::new_v4(), node:"audio/test".into(), instance:"one".into(), path:Mutex::new(path.clone()), retired_paths:Mutex::new(Vec::new()),
            state:Mutex::new(State { phase:"uploading",bytes:4,hash:None,error:None }),operation:tokio::sync::Mutex::new(()) });
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<InvokeRequest>(2);
        d.kernel().attach_peer(crate::peers::PeerConfig { nick:"audio/test".into(),instance:"one".into(),principal:None },Some(sender)).await.unwrap();
        let cas = d.kernel().cas().clone();
        let id = job.id;
        let expected = hash.clone();
        let daemon = tokio::spawn(async move {
            let request = receiver.recv().await.unwrap();
            assert_eq!(request.action, "keep_status");
            request.reply.send(crate::peers::InvokeResponse { result:Ok(serde_json::to_vec(&json!({"id":id,"state":"uploaded","bytes":4,"hash":expected,"upload_path":path})).unwrap()) }).unwrap();
            let request = receiver.recv().await.unwrap();
            assert_eq!(request.action, "keep_release");
            assert!(cas.exists(&expected), "CAS must contain verified material before release");
            request.reply.send(crate::peers::InvokeResponse { result:Ok(serde_json::to_vec(&json!({"id":id,"state":"released"})).unwrap()) }).unwrap();
        });
        advance(d.kernel(), &job).await.unwrap();
        advance(d.kernel(), &job).await.unwrap();
        daemon.await.unwrap();
        assert_eq!(job.state.lock().phase,"complete");
        assert!(!dir.exists());
        assert!(d.kernel().cas().exists(&hash));
    }

    #[tokio::test]
    async fn cancellation_keeps_staging_until_writer_acknowledges_stop() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("owned");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("capture.json");
        std::fs::write(&path, b"part").unwrap();
        let job = Arc::new(Job { id:Uuid::new_v4(),node:"audio/test".into(),instance:"one".into(),path:Mutex::new(path.clone()),retired_paths:Mutex::new(Vec::new()),
            state:Mutex::new(State { phase:"uploading",bytes:4,hash:None,error:None }),operation:tokio::sync::Mutex::new(()) });
        d.audio_keeps.insert(job.clone()).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<InvokeRequest>(2);
        d.kernel().attach_peer(crate::peers::PeerConfig { nick:"audio/test".into(),instance:"one".into(),principal:None },Some(sender)).await.unwrap();
        let id = job.id;
        let remote_path = path.clone();
        let daemon = tokio::spawn(async move {
            for (action, state) in [("keep_cancel","cancelling"),("keep_status","cancelled")] {
                let request = receiver.recv().await.unwrap();
                assert_eq!(request.action, action);
                request.reply.send(crate::peers::InvokeResponse { result:Ok(serde_json::to_vec(&json!({"id":id,"state":state,"bytes":4,"upload_path":remote_path})).unwrap()) }).unwrap();
            }
        });
        assert!(d.audio_keep_cancel(id).await.is_ok());
        assert!(path.exists(), "writer has not acknowledged cancellation yet");
        advance(d.kernel(), &job).await.unwrap();
        daemon.await.unwrap();
        assert_eq!(job.state.lock().phase,"cancelled");
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_is_idempotent_after_acknowledged_release() {
        let root = tempfile::tempdir().unwrap();
        let owned = root.path().join("owned");
        std::fs::create_dir(&owned).unwrap();
        let path = owned.join("capture.json");
        std::fs::write(&path, b"take").unwrap();
        cleanup(&path).unwrap();
        cleanup(&path).expect("retry after cleanup must not strand a released job");
        assert!(root.path().is_dir());
    }

    #[test]
    fn streaming_accept_checks_length_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cas = kaijutsu_cas::FileStore::at_path(dir.path().join("cas"));
        let path = dir.path().join("take.json");
        std::fs::write(&path, b"take").unwrap();
        let hash = kaijutsu_cas::ContentHash::from_data(b"take").to_string();
        assert!(accept_file(&cas, &path, 3, &hash).is_err());
        assert!(accept_file(&cas, &path, 4, &kaijutsu_cas::ContentHash::from_data(b"wrong").to_string()).is_err());
        assert_eq!(accept_file(&cas, &path, 4, &hash).unwrap(), hash);
        #[cfg(unix)] {
            let link = dir.path().join("link.json");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(accept_file(&cas, &link, 4, &hash).is_err());
        }
    }

    #[test]
    fn replies_must_match_id_and_bound_bytes() {
        let id = Uuid::new_v4();
        assert!(validate_reply(&json!({"id":Uuid::new_v4(),"bytes":1}), id).is_err());
        assert!(validate_reply(&json!({"id":id,"bytes":MAX_BYTES+1}), id).is_err());
        assert_eq!(validate_reply(&json!({"id":id,"bytes":3}), id).unwrap(), 3);
    }

    #[test]
    fn active_jobs_are_bounded() {
        let jobs = KeepJobs::default();
        for index in 0..=MAX_ACTIVE {
            let job = Arc::new(Job { id: Uuid::new_v4(), node: "audio/test".into(), instance: "test".into(), path: Mutex::new(PathBuf::new()), retired_paths: Mutex::new(Vec::new()),
                state: Mutex::new(State { phase: "uploading", bytes:0,hash:None,error:None }), operation: tokio::sync::Mutex::new(()) });
            assert!(reported_path(&job, &json!({"upload_path":"/etc/passwd"})).is_err());
            assert_eq!(jobs.insert(job).is_ok(), index < MAX_ACTIVE);
        }
    }
}

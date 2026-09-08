//! Bounded RAM reservations and asynchronous export for requested MIDI windows.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use uuid::Uuid;

const MAX_TAKES: usize = 8;
const MAX_RECORDS: usize = 32;
const MAX_TAKE_BYTES: usize = 16 * 1024 * 1024;
const MAX_POOL_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeepRequest {
    pub id: Uuid,
    pub source: String,
    pub generation: Uuid,
    pub seconds: u32,
    pub upload_path: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TakeStatus {
    pub id: Uuid,
    pub state: &'static str,
    pub bytes: usize,
    pub hash: Option<String>,
    pub error: Option<String>,
    pub protection: &'static str,
    pub upload_path: String,
}

struct Take {
    request: KeepRequest,
    status: TakeStatus,
    payload: Option<Arc<[u8]>>,
    charge: usize,
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
struct Pool { takes: BTreeMap<Uuid, Take> }

impl Pool {
    fn reserve(&mut self, request: KeepRequest) -> Result<bool, String> {
        if let Some(take) = self.takes.get(&request.id) {
            if take.request != request { return Err("keep id already belongs to a different request".into()); }
            return Ok(false);
        }
        let active = self.takes.values().filter(|take| take.charge > 0).count();
        let bytes: usize = self.takes.values().map(|take| take.charge).sum();
        if active >= MAX_TAKES || bytes > MAX_POOL_BYTES - MAX_TAKE_BYTES {
            return Err("RAM take budget exhausted; release or cancel an existing take".into());
        }
        if self.takes.len() >= MAX_RECORDS {
            let old = self.takes.iter().find(|(_, take)| take.charge == 0).map(|(id, _)| *id)
                .ok_or("take status capacity exhausted")?;
            self.takes.remove(&old);
        }
        let (cancel, _) = watch::channel(false);
        self.takes.insert(request.id, Take {
            status: TakeStatus { id: request.id, state: "preparing", bytes: 0, hash: None, error: None, protection: "ram", upload_path: request.upload_path.clone() },
            request, payload: None, charge: MAX_TAKE_BYTES, cancel,
        });
        Ok(true)
    }

    fn install(&mut self, id: Uuid, payload: Arc<[u8]>) -> Result<(), String> {
        if payload.len() > MAX_TAKE_BYTES { return Err("encoded take exceeds 16 MiB; request a shorter window".into()); }
        let take = self.takes.get_mut(&id).ok_or("keep reservation disappeared")?;
        if take.status.state != "preparing" { return Err("keep was cancelled before reservation completed".into()); }
        take.status.bytes = payload.len();
        take.charge = payload.len();
        take.payload = Some(payload);
        take.status.state = "uploading";
        Ok(())
    }
}

#[derive(Clone)]
pub struct CaptureControl {
    observation: Option<Arc<Mutex<crate::observer::Observation>>>,
    ssh: kaijutsu_client::SshConfig,
    pool: Arc<Mutex<Pool>>,
    node: String,
    instance: String,
    /// Read at keep time so the artifact carries the offset its window's
    /// stamps were minted with (`docs/midi.md` "The one timebase") — a reader
    /// reconciling wallclock anchors later needs to know which clock they are.
    clock: kaijutsu_client::KernelClockHandle,
}

impl CaptureControl {
    pub fn new(engine: &crate::Engine, ssh: kaijutsu_client::SshConfig, node: String, instance: String) -> Self {
        Self { observation: engine.observation.clone(), ssh, pool: Arc::new(Mutex::new(Pool::default())), node, instance, clock: engine.clock.clone() }
    }

    pub fn shutdown(&self) {
        for take in self.pool.lock().expect("take pool lock poisoned").takes.values() { let _ = take.cancel.send(true); }
    }

    pub async fn handle(&self, action: &str, params: &[u8]) -> Result<Vec<u8>, String> {
        let value: serde_json::Value = match action {
            "inventory" => {
                let mut inventory = match &self.observation {
                    Some(observation) => observation.lock().expect("MIDI observation lock poisoned").inventory(),
                    None => serde_json::json!({"backend":"alsa","state":"disabled","ports":[],"sources":[]}),
                };
                inventory["node"] = self.node.clone().into();
                inventory["instance"] = self.instance.clone().into();
                inventory
            }
            "keep" => {
                let request: KeepRequest = serde_json::from_slice(params).map_err(|e| e.to_string())?;
                validate_request(&request)?;
                self.keep(request).await?
            }
            "keep_status" | "keep_cancel" | "keep_release" | "keep_retry" => {
                let params: serde_json::Value = serde_json::from_slice(params).map_err(|e| e.to_string())?;
                let id: Uuid = params.get("id").and_then(|v| v.as_str()).ok_or("keep id is required")?
                    .parse().map_err(|_| "invalid keep id")?;
                if action == "keep_retry" {
                    let path = params.get("upload_path").and_then(|v| v.as_str()).ok_or("upload_path is required")?;
                    validate_path(path)?;
                    {
                        let mut pool = self.pool.lock().expect("take pool lock poisoned");
                        let take = pool.takes.get_mut(&id).ok_or("unknown keep id; RAM takes do not survive daemon restart")?;
                        if take.status.state != "failed" { return Err("only a failed upload can be retried".into()); }
                        if path == take.request.upload_path { return Err("retry requires a fresh staging path".into()); }
                        take.request.upload_path = path.to_string();
                        take.status.upload_path = path.to_string();
                        take.cancel = watch::channel(false).0;
                        take.status.state = "uploading";
                        take.status.error = None;
                    }
                    self.start_upload(id)?;
                }
                let mut pool = self.pool.lock().expect("take pool lock poisoned");
                if action == "keep_cancel" && !pool.takes.contains_key(&id) {
                    return serde_json::to_vec(&serde_json::json!({"id":id,"state":"cancelled","bytes":0,"protection":"none"}))
                        .map_err(|e| e.to_string());
                }
                let take = pool.takes.get_mut(&id).ok_or("unknown keep id; RAM takes do not survive daemon restart")?;
                if action == "keep_release" {
                    let hash = params.get("hash").and_then(|v| v.as_str()).ok_or("accepted hash is required")?;
                    if !matches!(take.status.state, "uploaded" | "released") || take.status.hash.as_deref() != Some(hash) {
                        return Err("keep release requires the uploaded hash accepted by the kernel".into());
                    }
                    take.payload = None;
                    take.charge = 0;
                    take.status.state = "released";
                    take.status.protection = "none";
                } else if action == "keep_cancel" {
                    if matches!(take.status.state, "preparing" | "uploading" | "cancelling") {
                        take.status.state = "cancelling";
                        let _ = take.cancel.send(true);
                    } else {
                        take.payload = None;
                        take.charge = 0;
                        take.status.state = "cancelled";
                        take.status.protection = "none";
                    }
                }
                serde_json::to_value(&take.status).map_err(|e| e.to_string())?
            }
            _ => return Err(format!("unknown audio peer action '{action}'; use status, inventory, keep or keep_status")),
        };
        serde_json::to_vec(&value).map_err(|e| e.to_string())
    }

    async fn keep(&self, request: KeepRequest) -> Result<serde_json::Value, String> {
        if self.observation.is_none() { return Err("MIDI is disabled".into()); }
        let inserted = self.pool.lock().expect("take pool lock poisoned").reserve(request.clone())?;
        if !inserted {
            return serde_json::to_value(&self.pool.lock().expect("take pool lock poisoned").takes[&request.id].status)
                .map_err(|e| e.to_string());
        }
        // The request owns preparation; dropping an RPC waiter must not strand
        // its charged reservation while a blocking snapshot is still running.
        let owner = self.clone();
        let (reply, result) = tokio::sync::oneshot::channel();
        tokio::spawn(async move { let _ = reply.send(owner.prepare(request).await); });
        result.await.map_err(|_| "keep preparation task stopped".to_string())?
    }

    async fn prepare(&self, request: KeepRequest) -> Result<serde_json::Value, String> {
        let observation = self.observation.clone().ok_or("MIDI is disabled")?;
        let selection = request.clone();
        let node = self.node.clone();
        let instance = self.instance.clone();
        let clock = self.clock.snapshot();
        let result = tokio::task::spawn_blocking(move || {
            let (window, epoch) = {
                let mut observation = observation.lock().expect("MIDI observation lock poisoned");
                let (head, epoch) = observation.head.ok_or("MIDI input has not reported a complete read head")?;
                if head.elapsed() > std::time::Duration::from_secs(1) { return Err("MIDI read head is stale; input may not be keeping up".into()); }
                let window = observation.history.keep(&selection.source, selection.generation, selection.seconds as f64, head)?;
                (window, epoch)
            };
            #[derive(Serialize)]
            struct Artifact {
                v: u32, kind: &'static str, request_id: Uuid, node: String, instance: String,
                window_end_ns: u64, window_start_ns: u64, clock: kaijutsu_audio::ClockSnapshot,
                window: crate::history::HistoryWindow,
            }
            let artifact = Artifact { v: 1, kind: "midi-history", request_id: selection.id, node, instance,
                window_end_ns: epoch, window_start_ns: epoch.saturating_sub(selection.seconds as u64 * 1_000_000_000), clock, window };
            let mut writer = LimitedWriter(Vec::new());
            serde_json::to_writer(&mut writer, &artifact).map_err(|e| e.to_string())?;
            Ok::<Arc<[u8]>, String>(writer.0.into())
        }).await.unwrap_or_else(|e| Err(format!("keep worker failed: {e}")));
        let payload = match result {
            Ok(payload) => payload,
            Err(error) => {
                self.pool.lock().expect("take pool lock poisoned").takes.remove(&request.id);
                return Err(error);
            }
        };
        {
            let mut pool = self.pool.lock().expect("take pool lock poisoned");
            if let Err(error) = pool.install(request.id, payload) {
                if let Some(take) = pool.takes.get_mut(&request.id) {
                    take.charge = 0; take.status.state = "cancelled";
                    take.status.protection = "none";
                }
                return Err(error);
            }
        }
        self.start_upload(request.id)?;
        serde_json::to_value(&self.pool.lock().expect("take pool lock poisoned").takes[&request.id].status)
            .map_err(|e| e.to_string())
    }

    fn start_upload(&self, id: Uuid) -> Result<(), String> {
        let (payload, path, cancel) = {
            let pool = self.pool.lock().expect("take pool lock poisoned");
            let take = pool.takes.get(&id).ok_or("unknown keep id")?;
            (take.payload.clone().ok_or("keep has no protected material")?, take.request.upload_path.clone(), take.cancel.subscribe())
        };
        let pool = self.pool.clone();
        let ssh = self.ssh.clone();
        tokio::spawn(async move {
            let result = crate::capture_export::upload(ssh, path, payload, cancel).await;
            let mut pool = pool.lock().expect("take pool lock poisoned");
            if let Some(take) = pool.takes.get_mut(&id) {
                if take.status.state == "cancelling" {
                    take.payload = None; take.charge = 0; take.status.state = "cancelled";
                    take.status.protection = "none";
                } else {
                    match result {
                        Ok(upload) => {
                            if upload.bytes != take.status.bytes as u64 {
                                take.status.state = "failed"; take.status.error = Some("uploaded take length mismatch".into());
                            } else {
                                take.status.state = "uploaded"; take.status.hash = Some(upload.hash.to_string());
                            }
                        }
                        Err(error) => { take.status.state = "failed"; take.status.error = Some(error); }
                    }
                }
            }
        });
        Ok(())
    }
}

fn validate_request(request: &KeepRequest) -> Result<(), String> {
    if !(1..=60).contains(&request.seconds) { return Err("keep seconds must be 1–60".into()); }
    if request.source.len() > 256 { return Err("source address is too long".into()); }
    validate_path(&request.upload_path)
}

fn validate_path(path: &str) -> Result<(), String> {
    let token = path.strip_prefix("/tmp/kaijutsu-audio-").and_then(|p| p.strip_suffix("/capture.json"))
        .ok_or("keep requires a kernel-owned capture staging path")?;
    let id: Uuid = token.parse().map_err(|_| "invalid capture staging id")?;
    if id.to_string() != token { return Err("capture staging id must be canonical".into()); }
    Ok(())
}

struct LimitedWriter(Vec<u8>);
impl std::io::Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_TAKE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("encoded take exceeds 16 MiB; request a shorter window"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> KeepRequest {
        KeepRequest { id: Uuid::new_v4(), source: "24:0".into(), generation: Uuid::new_v4(), seconds: 10,
            upload_path: format!("/tmp/kaijutsu-audio-{}/capture.json", Uuid::new_v4()) }
    }

    #[test]
    fn duplicate_keep_reserves_only_once_and_changed_request_fails() {
        let mut pool = Pool::default();
        let mut request = request();
        assert!(pool.reserve(request.clone()).unwrap());
        assert!(!pool.reserve(request.clone()).unwrap());
        request.seconds += 1;
        assert!(pool.reserve(request).is_err());
    }

    #[test]
    fn preparing_requests_are_charged_before_allocating_snapshots() {
        let mut pool = Pool::default();
        pool.reserve(request()).unwrap();
        pool.reserve(request()).unwrap();
        assert!(pool.reserve(request()).is_err());
    }

    #[test]
    fn owned_payload_remains_protected_until_release() {
        let mut pool = Pool::default();
        let request = request();
        pool.reserve(request.clone()).unwrap();
        pool.install(request.id, Arc::from(&b"take"[..])).unwrap();
        let take = pool.takes.get(&request.id).unwrap();
        assert_eq!(take.status.state, "uploading");
        assert_eq!(take.charge, 4);
        assert_eq!(take.payload.as_deref(), Some(&b"take"[..]));
    }

    fn controller() -> CaptureControl {
        CaptureControl { observation: None, ssh: kaijutsu_client::SshConfig::default(), clock: kaijutsu_client::KernelClockHandle::new(), pool: Arc::new(Mutex::new(Pool::default())), node:"audio/test".into(), instance:"test".into() }
    }

    #[tokio::test]
    async fn cancel_unknown_is_idempotent_and_release_requires_accepted_hash() {
        let control = controller();
        let request = request();
        let cancel = serde_json::to_vec(&serde_json::json!({"id":request.id})).unwrap();
        let response: serde_json::Value = serde_json::from_slice(&control.handle("keep_cancel", &cancel).await.unwrap()).unwrap();
        assert_eq!(response["state"], "cancelled");
        {
            let mut pool = control.pool.lock().unwrap();
            pool.reserve(request.clone()).unwrap();
            pool.install(request.id, Arc::from(&b"take"[..])).unwrap();
            let take = pool.takes.get_mut(&request.id).unwrap();
            take.status.state = "uploaded";
            take.status.hash = Some("accepted".into());
        }
        let wrong = serde_json::to_vec(&serde_json::json!({"id":request.id,"hash":"wrong"})).unwrap();
        assert!(control.handle("keep_release", &wrong).await.is_err());
        assert_eq!(control.pool.lock().unwrap().takes[&request.id].charge, 4);
        let release = serde_json::to_vec(&serde_json::json!({"id":request.id,"hash":"accepted"})).unwrap();
        for _ in 0..2 { control.handle("keep_release", &release).await.unwrap(); }
        assert_eq!(control.pool.lock().unwrap().takes[&request.id].charge, 0);
        assert_eq!(control.pool.lock().unwrap().takes[&request.id].status.protection, "none");
    }

    #[tokio::test]
    async fn cancelled_upload_retains_budget_until_worker_finishes() {
        let control = controller();
        let request = request();
        {
            let mut pool = control.pool.lock().unwrap();
            pool.reserve(request.clone()).unwrap();
            pool.install(request.id, Arc::from(&b"take"[..])).unwrap();
        }
        let cancel = serde_json::to_vec(&serde_json::json!({"id":request.id})).unwrap();
        control.handle("keep_cancel", &cancel).await.unwrap();
        let pool = control.pool.lock().unwrap();
        assert_eq!(pool.takes[&request.id].status.state, "cancelling");
        assert_eq!(pool.takes[&request.id].charge, 4);
    }

    #[tokio::test]
    async fn dropping_waiter_does_not_strand_preparation_budget() {
        let mut control = controller();
        let observation = Arc::new(Mutex::new(crate::observer::Observation::new()));
        control.observation = Some(observation.clone());
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (unlock_tx, unlock_rx) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let _guard = observation.lock().unwrap();
            locked_tx.send(()).unwrap();
            unlock_rx.recv().unwrap();
        });
        locked_rx.recv().unwrap();
        let request = request();
        let worker_control = control.clone();
        let id = request.id;
        let waiter = tokio::spawn(async move { worker_control.keep(request).await });
        while !control.pool.lock().unwrap().takes.contains_key(&id) { tokio::task::yield_now().await; }
        waiter.abort();
        let _ = waiter.await;
        unlock_tx.send(()).unwrap();
        blocker.join().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while control.pool.lock().unwrap().takes.contains_key(&id) {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }).await.unwrap();
    }
}

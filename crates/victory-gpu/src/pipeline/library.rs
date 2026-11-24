//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with are listed by a key
//!    known when the title creates them (their shader containers and
//!    graphics state); the next run hands those to the workers at
//!    creation, during loading.
//!
//! All library calls are serialised: the interface does not promise that
//! stores, loads and serialising may overlap.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the pipeline library and the early list in `dir` from now on.
pub fn use_pipeline_cache(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    _blob: Vec<u8>,
    /// Pipelines stored since the last save.
    added: u64,
}

// SAFETY: D3D12 objects are free-threaded; every call is made under the
// mutex that holds the library.
unsafe impl Send for Library {}

const LIBRARY_FILE: &str = "host-pipelines.bin";
const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most (sized for full
/// open-world pipeline counts while keeping memory bounded).
const EARLY_MAX: usize = 65_536;
/// Set whenever a new pipeline is stored into the library since the last save.
static LIBRARY_DIRTY: AtomicBool = AtomicBool::new(false);
/// Prevents overlapping periodic saves from queuing redundant work.
static SAVE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn library() -> Option<&'static Mutex<Library>> {
    static LIBRARY: OnceLock<Option<Mutex<Library>>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let blob = DIR
                .get()
                .and_then(|d| std::fs::read(d.join(LIBRARY_FILE)).ok())
                .unwrap_or_default();
            let library = open(&device, blob);
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
            }
            library.map(Mutex::new)
        })
        .as_ref()
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, blob: Vec<u8>) -> Option<Library> {
    let make = |bytes: &[u8]| -> windows::core::Result<ID3D12PipelineLibrary1> {
        // An empty library is asked for with no blob at all. An empty slice
        // still has an address, and vkd3d-proton refuses an address with no
        // bytes behind it, so under Wine no library was ever kept.
        let blob = if bytes.is_empty() { std::ptr::null() } else { bytes.as_ptr().cast() };
        let mut library = std::ptr::null_mut();
        // SAFETY: the bytes outlive the library (kept beside it); on success
        // the host has written an owned ID3D12PipelineLibrary.
        unsafe {
            (Interface::vtable(device).CreatePipelineLibrary)(
                Interface::as_raw(device),
                blob,
                bytes.len(),
                &ID3D12PipelineLibrary::IID,
                &mut library,
            )
            .ok()?;
            ID3D12PipelineLibrary::from_raw(library).cast()
        }
    };
    if !blob.is_empty() {
        match make(&blob) {
            Ok(library) => {
                tracing::info!(bytes = blob.len(), "host pipeline library loaded");
                return Some(Library {
                    library,
                    _blob: blob,
                    added: 0,
                });
            }
            Err(e) => {
                tracing::info!(error = %e, "host pipeline library rejected; starting an empty one")
            }
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    let library = make(&[]).ok()?;
    Some(Library {
        library,
        _blob: Vec::new(),
        added: 0,
    })
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root]
        .into_iter()
        .chain(code.iter().copied())
        .chain([state.as_bytes()])
    {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored.
pub(crate) fn pipeline(
    name: &str,
    load: impl FnOnce(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let wide = HSTRING::from(name);
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    if let Ok(pso) = load(
        &library.lock().unwrap_or_else(|e| e.into_inner()).library,
        PCWSTR(wide.as_ptr()),
    ) {
        LOADED.fetch_add(1, Ordering::Relaxed);
        return Ok(pso);
    }
    // Compiled outside the lock: compiles take up to seconds.
    let pso = create()?;
    MADE.fetch_add(1, Ordering::Relaxed);
    let mut l = library.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a live pipeline; a name already stored (a description that
    // differs) is refused, which leaves the library as it was.
    if unsafe { l.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) }.is_ok() {
        l.added += 1;
        LIBRARY_DIRTY.store(true, Ordering::Relaxed);
    }
    Ok(pso)
}

struct SaveAck {
    done: Mutex<bool>,
    cv: Condvar,
}

impl SaveAck {
    fn new() -> Self {
        Self {
            done: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn signal(&self) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        *done = true;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            done = self.cv.wait(done).unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct SaveRequest {
    dir: PathBuf,
    ack: Option<Arc<SaveAck>>,
}

/// Dedicated background saver thread so periodic saves never block PSO
/// compilation workers on serialization or disk I/O, while shutdown (`now =
/// true`) can still wait for a clean flush.
fn saver() -> Option<&'static mpsc::Sender<SaveRequest>> {
    static SAVER: OnceLock<Option<mpsc::Sender<SaveRequest>>> = OnceLock::new();
    SAVER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<SaveRequest>();
            let spawned = std::thread::Builder::new()
                .name("victory-pipeline-save".into())
                .spawn(move || {
                    // SAFETY: plain priority call on our own background thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    let mut buf = Vec::new();
                    while let Ok(req) = rx.recv() {
                        perform_save(&req.dir, &mut buf);
                        SAVE_IN_FLIGHT.store(false, Ordering::Release);
                        if let Some(ack) = req.ack {
                            ack.signal();
                        }
                    }
                });
            match spawned {
                Ok(_) => Some(tx),
                Err(e) => {
                    tracing::warn!(error = %e, "no pipeline saver thread");
                    None
                }
            }
        })
        .as_ref()
}

/// Serializes the library into `buf` (growing `buf` outside `library.lock()` so
/// multi-megabyte page faults never stall PSO workers), returning the serialized
/// byte length and count of newly stored pipelines.
fn serialize_library(library: &Mutex<Library>, buf: &mut Vec<u8>) -> Option<(usize, u64)> {
    let needed = {
        let l = library.lock().unwrap_or_else(|e| e.into_inner());
        if l.added == 0 {
            LIBRARY_DIRTY.store(false, Ordering::Relaxed);
            return None;
        }
        // SAFETY: plain size query on a live pipeline library under its mutex.
        unsafe { l.library.GetSerializedSize() }
    };
    if needed == 0 {
        return None;
    }
    if buf.len() < needed {
        buf.resize(needed.saturating_add(needed / 16).max(needed), 0);
    }
    loop {
        let mut l = library.lock().unwrap_or_else(|e| e.into_inner());
        if l.added == 0 {
            LIBRARY_DIRTY.store(false, Ordering::Relaxed);
            return None;
        }
        // SAFETY: plain size query on a live pipeline library under its mutex.
        let exact = unsafe { l.library.GetSerializedSize() };
        if exact > buf.len() {
            drop(l);
            buf.resize(exact.saturating_add(exact / 16).max(exact), 0);
            continue;
        }
        // SAFETY: `buf[..exact]` has the exact size requested by `GetSerializedSize`.
        match unsafe { l.library.Serialize(&mut buf[..exact]) } {
            Ok(()) => {
                let added = std::mem::take(&mut l.added);
                LIBRARY_DIRTY.store(false, Ordering::Relaxed);
                return Some((exact, added));
            }
            Err(e) => {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                return None;
            }
        }
    }
}

/// Performs the actual snapshot + unlocked disk I/O for `host-pipelines.bin`
/// and `early-pipelines.txt`.
fn perform_save(dir: &Path, buf: &mut Vec<u8>) {
    let Some(library) = library() else { return };
    let serialized = serialize_library(library, buf);
    let used_snapshot = if USED_CHANGED.swap(false, Ordering::AcqRel) {
        let keys: Vec<String> = USED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flatten()
            .cloned()
            .collect();
        Some(keys)
    } else {
        None
    };
    if serialized.is_none() && used_snapshot.is_none() {
        return;
    }

    if let Err(e) = std::fs::create_dir_all(dir) {
        if let Some((_, added)) = serialized {
            library.lock().unwrap_or_else(|e| e.into_inner()).added += added;
            LIBRARY_DIRTY.store(true, Ordering::Relaxed);
        }
        if used_snapshot.is_some() {
            USED_CHANGED.store(true, Ordering::Relaxed);
        }
        tracing::warn!(error = %e, "host pipeline library directory not created");
        return;
    }

    let mut lib_bytes = 0usize;
    let mut ok = true;
    if let Some((len, added)) = serialized {
        if let Err(e) = write_atomically(&dir.join(LIBRARY_FILE), &buf[..len]) {
            library.lock().unwrap_or_else(|e| e.into_inner()).added += added;
            LIBRARY_DIRTY.store(true, Ordering::Relaxed);
            tracing::warn!(error = %e, "host pipeline library not saved");
            ok = false;
        } else {
            lib_bytes = len;
        }
    }
    let mut early_count = 0usize;
    if let Some(used) = used_snapshot {
        early_count = used.len();
        if let Err(e) = write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()) {
            USED_CHANGED.store(true, Ordering::Relaxed);
            tracing::warn!(error = %e, "early pipeline list not saved");
            ok = false;
        }
    }
    if ok {
        tracing::info!(
            bytes = lib_bytes,
            early = early_count,
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipeline library saved"
        );
    }
}

/// Writes the library and the early list if pipelines were added since the
/// last save and `SAVE_EVERY` has passed (or `now`). Periodic saves run
/// asynchronously on the dedicated saver thread; `now = true` waits for disk
/// completion.
pub(crate) fn save(now: bool) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
    let Some(dir) = DIR.get() else { return };
    if !now && !LIBRARY_DIRTY.load(Ordering::Relaxed) && !USED_CHANGED.load(Ordering::Relaxed) {
        return;
    }
    if !now && SAVE_IN_FLIGHT.load(Ordering::Acquire) {
        return;
    }
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if !now && last.is_some_and(|t| t.elapsed() < SAVE_EVERY) {
            return;
        }
        if !now && SAVE_IN_FLIGHT.swap(true, Ordering::AcqRel) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    if let Some(tx) = saver() {
        let ack = now.then(|| Arc::new(SaveAck::new()));
        let req = SaveRequest {
            dir: dir.clone(),
            ack: ack.clone(),
        };
        if tx.send(req).is_ok() {
            if let Some(ack) = ack {
                ack.wait();
            }
            return;
        }
    }
    // Fallback if no background saver thread could be created.
    let mut buf = Vec::new();
    perform_save(dir, &mut buf);
    SAVE_IN_FLIGHT.store(false, Ordering::Release);
}

/// Writes `bytes` to a unique temporary file beside `path`, then renames it
/// over `path`, removing the temporary file if either write or rename fails.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("cache");
    let tmp = path.with_file_name(format!(".{file_name}.tmp-{}-{n}", std::process::id()));
    if let Err(err) = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Title pipeline keys drawn or dispatched with (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed
            .lines()
            .filter(|l| !l.is_empty())
            .take(EARLY_MAX)
            .map(str::to_owned)
            .collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was drawn with in an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// Inserts `key` into `set` if below [`EARLY_MAX`], returning `true` if newly
/// added.
fn record_used(set: &mut HashSet<String>, key: &str) -> bool {
    if set.len() < EARLY_MAX && !set.contains(key) {
        set.insert(key.to_owned())
    } else {
        false
    }
}

/// The title pipeline with `key` was drawn or dispatched with.
pub(crate) fn note_used(key: &str) {
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if record_used(set, key) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok())
        else {
            return; // no host GPU here
        };
        let library =
            open(&device, b"not a pipeline library".repeat(16)).expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(library._blob.is_empty());
    }

    #[test]
    fn early_list_records_and_deduplicates_above_old_8192_cap() {
        const { assert!(EARLY_MAX >= 65_536) };
        let mut set = HashSet::new();
        // Insert 10,000 distinct keys (exceeding the legacy 8,192 cap).
        for i in 0..10_000usize {
            let key = format!("{i:032x}");
            assert!(record_used(&mut set, &key));
            assert!(!record_used(&mut set, &key), "duplicates are deduplicated");
        }
        assert_eq!(set.len(), 10_000);

        // Fill up to EARLY_MAX and confirm the cap is enforced cleanly.
        for i in 10_000..EARLY_MAX {
            assert!(record_used(&mut set, &format!("{i:032x}")));
        }
        assert_eq!(set.len(), EARLY_MAX);
        assert!(
            !record_used(&mut set, "ffffffffffffffffffffffffffffffff"),
            "keys beyond EARLY_MAX are ignored"
        );
        assert_eq!(set.len(), EARLY_MAX);
    }

    #[test]
    fn write_atomically_overwrites_and_cleans_up_temp_file_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(LIBRARY_FILE);
        write_atomically(&target, b"first").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        write_atomically(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");

        // Trigger a rename failure by placing a directory at the destination path.
        let blocked = dir.path().join("blocked.bin");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_atomically(&blocked, b"payload").is_err());

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary staging files leaked on error: {leftovers:?}"
        );
    }
}

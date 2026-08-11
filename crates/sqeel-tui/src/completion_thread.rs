use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};

/// Runs schema completion filtering on a dedicated thread.
///
/// Submit `(prefix, identifiers, lowered)` with [`CompletionThread::submit`]; the
/// thread always processes the latest submitted triple (older pending triples are
/// discarded). `lowered` is the parallel lowercased form of `identifiers`, so the
/// per-name lowercase that used to happen here per keystroke happens once,
/// upstream, when the pool is built.
/// Poll completed results with [`CompletionThread::try_recv`].
type PendingSlot = Arc<(
    Mutex<Option<(String, Arc<Vec<String>>, Arc<Vec<String>>)>>,
    Condvar,
)>;

pub struct CompletionThread {
    pending: PendingSlot,
    result_rx: mpsc::Receiver<Vec<String>>,
}

impl CompletionThread {
    pub fn spawn() -> anyhow::Result<Self> {
        let pending: PendingSlot = Arc::new((Mutex::new(None), Condvar::new()));
        let (result_tx, result_rx) = mpsc::channel::<Vec<String>>();
        let pending_thread = Arc::clone(&pending);

        std::thread::Builder::new()
            .name("completion".into())
            .spawn(move || {
                let (lock, cvar) = &*pending_thread;
                loop {
                    let (prefix, identifiers, lowered) = {
                        let mut guard = lock.lock().unwrap_or_else(|p| p.into_inner());
                        loop {
                            match guard.take() {
                                Some(q) => break q,
                                None => {
                                    guard = cvar.wait(guard).unwrap_or_else(|p| p.into_inner());
                                }
                            }
                        }
                    };
                    let prefix_lower = prefix.to_lowercase();
                    // `identifiers` is already sorted + deduped upstream (AppState
                    // cache); `lowered` mirrors it, so filter on the pre-lowered
                    // copy instead of lowercasing every name per keystroke.
                    let results: Vec<String> = lowered
                        .iter()
                        .zip(identifiers.iter())
                        .filter(|(l, _)| l.starts_with(&prefix_lower))
                        .map(|(_, name)| name.clone())
                        .collect();
                    if result_tx.send(results).is_err() {
                        break;
                    }
                }
            })?;

        Ok(Self { pending, result_rx })
    }

    /// Submit a new completion query. Replaces any not-yet-processed triple.
    pub fn submit(&self, prefix: String, identifiers: Arc<Vec<String>>, lowered: Arc<Vec<String>>) {
        let (lock, cvar) = &*self.pending;
        *lock.lock().unwrap_or_else(|p| p.into_inner()) = Some((prefix, identifiers, lowered));
        cvar.notify_one();
    }

    /// Returns the latest completed result, if any.
    pub fn try_recv(&self) -> Option<Vec<String>> {
        let mut latest = None;
        while let Ok(items) = self.result_rx.try_recv() {
            latest = Some(items);
        }
        latest
    }
}

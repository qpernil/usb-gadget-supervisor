use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceMode {
    Batched(Duration),
    Immediate,
}

impl PersistenceMode {
    fn validate(self) -> io::Result<()> {
        match self {
            Self::Batched(delay) if delay.is_zero() => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "persistence batching delay must be nonzero",
            )),
            Self::Batched(_) | Self::Immediate => Ok(()),
        }
    }
}

pub struct StatePersistence<T> {
    handle: StatePersistenceHandle<T>,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}

pub struct StatePersistenceHandle<T> {
    state: Arc<Mutex<T>>,
    scheduling: Arc<Scheduling>,
}

impl<T> Clone for StatePersistenceHandle<T> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            scheduling: Arc::clone(&self.scheduling),
        }
    }
}

pub struct MutationReceipt {
    scheduling: Arc<Scheduling>,
    mutation_epoch: u64,
    wait_for_persistence: bool,
}

struct Scheduling {
    mode: PersistenceMode,
    state: Mutex<ScheduleState>,
    changed: Condvar,
}

#[derive(Default)]
struct ScheduleState {
    mutation_epoch: u64,
    persisted_epoch: u64,
    deadline: Option<Instant>,
    force: bool,
    stopping: bool,
    failure: Option<StoredError>,
}

#[derive(Clone)]
struct StoredError {
    kind: io::ErrorKind,
    message: String,
}

impl StoredError {
    fn capture(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn to_io_error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

impl<T: Send + 'static> StatePersistence<T> {
    pub fn start<E, F>(
        state: T,
        path: PathBuf,
        mode: PersistenceMode,
        encode: E,
        on_failure: F,
    ) -> io::Result<Self>
    where
        E: Fn(&T) -> io::Result<Vec<u8>> + Send + 'static,
        F: Fn() + Send + 'static,
    {
        Self::start_with_writer(
            state,
            mode,
            encode,
            move |encoded| replace_file_atomically(&path, encoded),
            on_failure,
        )
    }

    fn start_with_writer<E, W, F>(
        state: T,
        mode: PersistenceMode,
        encode: E,
        write: W,
        on_failure: F,
    ) -> io::Result<Self>
    where
        E: Fn(&T) -> io::Result<Vec<u8>> + Send + 'static,
        W: Fn(&[u8]) -> io::Result<()> + Send + 'static,
        F: Fn() + Send + 'static,
    {
        mode.validate()?;
        let state = Arc::new(Mutex::new(state));
        let scheduling = Arc::new(Scheduling {
            mode,
            state: Mutex::new(ScheduleState::default()),
            changed: Condvar::new(),
        });
        let thread_state = Arc::clone(&state);
        let thread_scheduling = Arc::clone(&scheduling);
        let thread = thread::Builder::new()
            .name("state-persistence".to_owned())
            .spawn(move || {
                let result = run_persistence(thread_state, &thread_scheduling, &encode, &write);
                if let Err(error) = &result {
                    let mut schedule = lock(&thread_scheduling.state);
                    schedule.failure = Some(StoredError::capture(error));
                    thread_scheduling.changed.notify_all();
                    drop(schedule);
                    on_failure();
                }
                result
            })?;
        Ok(Self {
            handle: StatePersistenceHandle { state, scheduling },
            thread: Some(thread),
        })
    }
}

impl<T> StatePersistence<T> {
    pub fn handle(&self) -> StatePersistenceHandle<T> {
        self.handle.clone()
    }

    pub fn flush(&self) -> io::Result<()> {
        self.handle.flush()
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        self.request_stop();
        self.join()
    }

    fn request_stop(&self) {
        let mut schedule = lock(&self.handle.scheduling.state);
        schedule.stopping = true;
        schedule.force = true;
        self.handle.scheduling.changed.notify_all();
    }

    fn join(&mut self) -> io::Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| io::Error::other("state persistence thread panicked"))?
    }
}

impl<T> Drop for StatePersistence<T> {
    fn drop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        self.request_stop();
        let _ = self.join();
    }
}

impl<T> StatePersistenceHandle<T> {
    pub fn state(&self) -> &Arc<Mutex<T>> {
        &self.state
    }

    /// Records one durable transaction while the caller still holds the state
    /// mutex. Drop that state guard before waiting on the returned receipt.
    pub fn record_mutation(&self) -> io::Result<MutationReceipt> {
        let mut schedule = lock(&self.scheduling.state);
        check_failure(&schedule)?;
        let mutation_epoch = schedule
            .mutation_epoch
            .checked_add(1)
            .ok_or_else(|| io::Error::other("persistence mutation epoch exhausted"))?;
        schedule.mutation_epoch = mutation_epoch;
        let wait_for_persistence = self.scheduling.mode == PersistenceMode::Immediate;
        match self.scheduling.mode {
            PersistenceMode::Batched(delay) => {
                if schedule.deadline.is_none() {
                    schedule.deadline = Some(Instant::now() + delay);
                }
            }
            PersistenceMode::Immediate => schedule.force = true,
        }
        self.scheduling.changed.notify_all();
        Ok(MutationReceipt {
            scheduling: Arc::clone(&self.scheduling),
            mutation_epoch,
            wait_for_persistence,
        })
    }

    pub fn flush(&self) -> io::Result<()> {
        let target = {
            let mut schedule = lock(&self.scheduling.state);
            check_failure(&schedule)?;
            let target = schedule.mutation_epoch;
            if schedule.persisted_epoch >= target {
                return Ok(());
            }
            schedule.force = true;
            self.scheduling.changed.notify_all();
            target
        };
        wait_for_epoch(&self.scheduling, target)
    }

    pub fn check_health(&self) -> io::Result<()> {
        check_failure(&lock(&self.scheduling.state))
    }
}

impl MutationReceipt {
    pub fn wait(self) -> io::Result<()> {
        if self.wait_for_persistence {
            wait_for_epoch(&self.scheduling, self.mutation_epoch)
        } else {
            check_failure(&lock(&self.scheduling.state))
        }
    }

    pub fn mutation_epoch(&self) -> u64 {
        self.mutation_epoch
    }
}

fn run_persistence<T, E, W>(
    state: Arc<Mutex<T>>,
    scheduling: &Scheduling,
    encode: &E,
    write: &W,
) -> io::Result<()>
where
    E: Fn(&T) -> io::Result<Vec<u8>>,
    W: Fn(&[u8]) -> io::Result<()>,
{
    loop {
        if !wait_until_due(scheduling)? {
            return Ok(());
        }
        let (encoded, snapshot_epoch) = {
            let state = lock(&state);
            let snapshot_epoch = lock(&scheduling.state).mutation_epoch;
            (encode(&state)?, snapshot_epoch)
        };
        write(&encoded)?;

        let mut schedule = lock(&scheduling.state);
        schedule.persisted_epoch = snapshot_epoch;
        if schedule.mutation_epoch == snapshot_epoch {
            schedule.deadline = None;
        } else if schedule.deadline.is_none() {
            schedule.deadline = match scheduling.mode {
                PersistenceMode::Batched(delay) => Some(Instant::now() + delay),
                PersistenceMode::Immediate => None,
            };
            if scheduling.mode == PersistenceMode::Immediate {
                schedule.force = true;
            }
        }
        scheduling.changed.notify_all();
    }
}

fn wait_until_due(scheduling: &Scheduling) -> io::Result<bool> {
    let mut schedule = lock(&scheduling.state);
    loop {
        check_failure(&schedule)?;
        if schedule.mutation_epoch == schedule.persisted_epoch {
            if schedule.stopping {
                return Ok(false);
            }
            schedule.force = false;
            schedule.deadline = None;
        } else if schedule.force
            || schedule.stopping
            || schedule
                .deadline
                .is_some_and(|deadline| deadline <= Instant::now())
        {
            schedule.force = false;
            schedule.deadline = None;
            return Ok(true);
        }

        schedule = match schedule.deadline {
            Some(deadline) => {
                let timeout = deadline.saturating_duration_since(Instant::now());
                scheduling
                    .changed
                    .wait_timeout(schedule, timeout)
                    .unwrap_or_else(|error| error.into_inner())
                    .0
            }
            None => scheduling
                .changed
                .wait(schedule)
                .unwrap_or_else(|error| error.into_inner()),
        };
    }
}

fn wait_for_epoch(scheduling: &Scheduling, target: u64) -> io::Result<()> {
    let mut schedule = lock(&scheduling.state);
    while schedule.persisted_epoch < target {
        check_failure(&schedule)?;
        schedule = scheduling
            .changed
            .wait(schedule)
            .unwrap_or_else(|error| error.into_inner());
    }
    Ok(())
}

fn check_failure(schedule: &ScheduleState) -> io::Result<()> {
    match &schedule.failure {
        Some(error) => Err(error.to_io_error()),
        None => Ok(()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

pub fn replace_file_atomically(path: &Path, encoded: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(encoded)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("persistent state has no parent directory"))?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

    #[test]
    fn batched_mutations_coalesce_without_extending_the_first_deadline() {
        let writes = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(Vec::new()));
        let writes_for_thread = Arc::clone(&writes);
        let last_for_thread = Arc::clone(&last);
        let persistence = StatePersistence::start_with_writer(
            0_u64,
            PersistenceMode::Batched(Duration::from_millis(80)),
            |value| Ok(value.to_be_bytes().to_vec()),
            move |encoded| {
                writes_for_thread.fetch_add(1, Ordering::Relaxed);
                *lock(&last_for_thread) = encoded.to_vec();
                Ok(())
            },
            || {},
        )
        .unwrap();
        let handle = persistence.handle();

        {
            let mut value = lock(handle.state());
            *value = 1;
            handle.record_mutation().unwrap();
        }
        thread::sleep(Duration::from_millis(60));
        {
            let mut value = lock(handle.state());
            *value = 2;
            handle.record_mutation().unwrap();
        }

        thread::sleep(Duration::from_millis(50));
        persistence.flush().unwrap();
        assert_eq!(writes.load(Ordering::Relaxed), 1);
        assert_eq!(*lock(&last), 2_u64.to_be_bytes());
        persistence.shutdown().unwrap();
    }

    #[test]
    fn mutation_during_a_write_schedules_the_latest_snapshot() {
        let writes = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let writes_for_thread = Arc::clone(&writes);
        let last_for_thread = Arc::clone(&last);
        let persistence = StatePersistence::start_with_writer(
            0_u64,
            PersistenceMode::Batched(Duration::from_millis(10)),
            |value| Ok(value.to_be_bytes().to_vec()),
            move |encoded| {
                if writes_for_thread.fetch_add(1, Ordering::Relaxed) == 0 {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                *lock(&last_for_thread) = encoded.to_vec();
                Ok(())
            },
            || {},
        )
        .unwrap();
        let handle = persistence.handle();
        {
            let mut value = lock(handle.state());
            *value = 1;
            handle.record_mutation().unwrap();
        }
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = {
            let mut value = lock(handle.state());
            *value = 2;
            handle.record_mutation().unwrap()
        };
        release_tx.send(()).unwrap();
        second.wait().unwrap();
        persistence.flush().unwrap();
        assert_eq!(writes.load(Ordering::Relaxed), 2);
        assert_eq!(*lock(&last), 2_u64.to_be_bytes());
        let _ = persistence.shutdown();
    }

    #[test]
    fn immediate_mode_waits_for_each_mutation() {
        let writes = Arc::new(AtomicUsize::new(0));
        let writes_for_thread = Arc::clone(&writes);
        let persistence = StatePersistence::start_with_writer(
            0_u64,
            PersistenceMode::Immediate,
            |value| Ok(value.to_be_bytes().to_vec()),
            move |_| {
                writes_for_thread.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            || {},
        )
        .unwrap();
        let handle = persistence.handle();
        for expected_epoch in 1..=3 {
            let receipt = {
                let mut value = lock(handle.state());
                *value = expected_epoch;
                handle.record_mutation().unwrap()
            };
            assert_eq!(receipt.mutation_epoch(), expected_epoch);
            receipt.wait().unwrap();
            assert_eq!(writes.load(Ordering::Relaxed), expected_epoch as usize);
        }
        let _ = persistence.shutdown();
    }

    #[test]
    fn persistence_failure_is_sticky_and_signalled() {
        let failed = Arc::new(AtomicBool::new(false));
        let failed_for_thread = Arc::clone(&failed);
        let persistence = StatePersistence::start_with_writer(
            0_u64,
            PersistenceMode::Immediate,
            |value| Ok(value.to_be_bytes().to_vec()),
            |_| Err(io::Error::other("injected persistence failure")),
            move || failed_for_thread.store(true, Ordering::Relaxed),
        )
        .unwrap();
        let handle = persistence.handle();
        let receipt = {
            let mut value = lock(handle.state());
            *value = 1;
            handle.record_mutation().unwrap()
        };
        assert!(receipt.wait().is_err());
        assert!(failed.load(Ordering::Relaxed));
        assert!(handle.record_mutation().is_err());
        assert!(persistence.shutdown().is_err());
    }

    #[test]
    fn shutdown_forces_a_pending_batch() {
        let writes = Arc::new(AtomicUsize::new(0));
        let writes_for_thread = Arc::clone(&writes);
        let persistence = StatePersistence::start_with_writer(
            0_u64,
            PersistenceMode::Batched(Duration::from_secs(30)),
            |value| Ok(value.to_be_bytes().to_vec()),
            move |_| {
                writes_for_thread.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            || {},
        )
        .unwrap();
        let handle = persistence.handle();
        {
            let mut value = lock(handle.state());
            *value = 1;
            handle.record_mutation().unwrap();
        }
        persistence.shutdown().unwrap();
        assert_eq!(writes.load(Ordering::Relaxed), 1);
    }
}

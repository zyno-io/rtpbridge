//! Fixed workers for blocking storage/decode jobs. Queued and running work is
//! bounded even when a caller times out: cancellation cannot kill a syscall.
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use tokio::sync::oneshot;

type Job = Box<dyn FnOnce() + Send + 'static>;

fn sender() -> &'static mpsc::SyncSender<Job> {
    static SENDER: OnceLock<mpsc::SyncSender<Job>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<Job>(64);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..4 {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("rtp-storage-{index}"))
                .spawn(move || {
                    loop {
                        let received = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        let Ok(job) = received else {
                            break;
                        };
                        // A malformed input must not permanently remove a worker.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                })
                .expect("start bounded storage worker");
        }
        sender
    })
}

pub async fn run<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let (reply, receiver) = oneshot::channel();
    sender()
        .try_send(Box::new(move || {
            if !reply.is_closed() {
                let _ = reply.send(work());
            }
        }))
        .map_err(|_| anyhow::anyhow!("STORAGE_BUSY"))?;
    let result = receiver.await;
    result.map_err(|_| anyhow::anyhow!("storage worker failed"))?
}

/// Bound the caller's wait without releasing resources owned by a running job.
pub async fn run_with_deadline<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let completed = tokio::time::timeout(std::time::Duration::from_secs(10), run(work)).await;
    completed.map_err(|_| anyhow::anyhow!("storage operation timed out"))?
}

/// Dropping these senders releases every worker, including when a test panics.
#[cfg(test)]
pub(crate) async fn block_workers() -> Vec<std::sync::mpsc::Sender<()>> {
    let mut releases = Vec::new();
    for _ in 0..4 {
        let (release, wait) = std::sync::mpsc::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        tokio::spawn(run(move || {
            let _ = started.send(());
            let _ = wait.recv();
            Ok(())
        }));
        let started = running.await;
        started.unwrap();
        releases.push(release);
    }
    releases
}

/// Limit admitted decoder streams independently of queued worker jobs. A permit
/// stays inside the decoder through cancellation of a pending blocking job.
pub fn admit_stream() -> anyhow::Result<tokio::sync::OwnedSemaphorePermit> {
    static STREAMS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    Arc::clone(STREAMS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(64))))
        .try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("PLAYBACK_BUSY"))
}

/// Walk relative components using directory handles. Symlinks and traversal are
/// rejected at every component, including the final open (no check/open race).
pub fn open_beneath(
    base: &std::path::Path,
    relative: &std::path::Path,
) -> anyhow::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    use std::path::Component;
    let mut directory = std::fs::File::open(base)?;
    let components: Vec<_> = relative.components().collect();
    anyhow::ensure!(!components.is_empty(), "empty path");
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            anyhow::bail!("path outside recording directory")
        };
        let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        if index + 1 < components.len() {
            flags |= OFlags::DIRECTORY;
        }
        directory = openat(&directory, *name, flags, Mode::empty())?.into();
    }
    anyhow::ensure!(
        directory.metadata()?.is_file(),
        "input must be a regular file"
    );
    Ok(directory)
}

/// Create a new regular file beneath `base` without following symlinks in any
/// relative component. The final `CREATE | EXCL` open preserves create-new
/// semantics while the directory-handle walk closes check/open races.
pub fn create_beneath(
    base: &std::path::Path,
    relative: &std::path::Path,
) -> anyhow::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    use std::path::Component;

    let mut directory = std::fs::File::open(base)?;
    let components: Vec<_> = relative.components().collect();
    anyhow::ensure!(!components.is_empty(), "empty path");
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            anyhow::bail!("path outside recording directory")
        };
        if index + 1 == components.len() {
            let flags =
                OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::CREATE | OFlags::EXCL;
            let file = openat(&directory, *name, flags, Mode::from_bits_truncate(0o666))?;
            return Ok(file.into());
        }

        let flags = OFlags::RDONLY
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::DIRECTORY;
        directory = openat(&directory, *name, flags, Mode::empty())?.into();
    }
    unreachable!("non-empty component list must return from its final component")
}

/// Remove a file beneath `base` using the same no-follow directory walk as
/// `open_beneath`. `unlinkat` acts on the final directory handle, so swapping a
/// parent pathname after validation cannot redirect the deletion.
pub fn remove_beneath(base: &std::path::Path, relative: &std::path::Path) -> anyhow::Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, unlinkat};
    use std::path::Component;

    let mut directory = std::fs::File::open(base)?;
    let components: Vec<_> = relative.components().collect();
    anyhow::ensure!(!components.is_empty(), "empty path");
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            anyhow::bail!("path outside recording directory")
        };
        if index + 1 == components.len() {
            unlinkat(&directory, *name, AtFlags::empty())?;
            return Ok(());
        }

        let flags = OFlags::RDONLY
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::DIRECTORY;
        directory = openat(&directory, *name, flags, Mode::empty())?.into();
    }
    unreachable!("non-empty component list must return from its final component")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelling_waiter_retains_real_worker_admission() {
        let budget = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::clone(&budget).try_acquire_owned().unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(run(move || {
            let _permit = permit;
            let _ = started.send(());
            wait.recv()?;
            Ok(())
        }));
        let started = running.await;
        started.unwrap();
        caller.abort();
        tokio::task::yield_now().await;
        assert_eq!(
            budget.available_permits(),
            0,
            "cancellation cannot release a running syscall's permit"
        );
        // The Tokio executor still runs while that worker is blocked.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        release.send(()).unwrap();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let acquired = budget.acquire().await;
            drop(acquired.unwrap());
        })
        .await;
        assert!(finished.is_ok());
    }

    #[test]
    fn path_walk_rejects_symlinks_and_parent_components() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("audio.pcap"), b"data").unwrap();
        std::os::unix::fs::symlink("audio.pcap", directory.path().join("link.pcap")).unwrap();
        assert!(open_beneath(directory.path(), std::path::Path::new("audio.pcap")).is_ok());
        assert!(open_beneath(directory.path(), std::path::Path::new("link.pcap")).is_err());
        assert!(open_beneath(directory.path(), std::path::Path::new("../audio.pcap")).is_err());
    }

    #[test]
    fn create_and_remove_walk_reject_symlinked_parents() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("redirect")).unwrap();

        let redirected = std::path::Path::new("redirect/recording.pcap");
        assert!(create_beneath(directory.path(), redirected).is_err());
        assert!(!outside.path().join("recording.pcap").exists());

        std::fs::write(outside.path().join("existing.pcap"), b"data").unwrap();
        assert!(
            remove_beneath(
                directory.path(),
                std::path::Path::new("redirect/existing.pcap")
            )
            .is_err()
        );
        assert!(outside.path().join("existing.pcap").exists());

        let created = std::path::Path::new("created.pcap");
        drop(create_beneath(directory.path(), created).unwrap());
        assert!(directory.path().join(created).is_file());
        remove_beneath(directory.path(), created).unwrap();
        assert!(!directory.path().join(created).exists());
    }
}

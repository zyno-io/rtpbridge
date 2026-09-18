//! Bounded in-memory ownership for a cross-session transfer.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc, oneshot};

use super::media_session::{EndpointTransferBundle, SessionCommand};
use crate::control::protocol::EndpointId;

#[derive(Debug, Default)]
pub struct TransferState {
    pub bundle: Option<Box<EndpointTransferBundle>>,
    /// Only this operation may release the source's rollback reservation.
    pub reserved: bool,
    pub cancelled: bool,
    pub committed: bool,
}

pub type TransferSlot = Arc<Mutex<TransferState>>;

/// Always schedule source cleanup, including on coordinator panic/cancellation.
/// The reserved channel slot makes this synchronous and reliable under backpressure.
struct FinishGuard {
    slot: TransferSlot,
    endpoint_id: EndpointId,
    finish: Option<mpsc::OwnedPermit<SessionCommand>>,
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        {
            let mut state = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            state.cancelled = !state.committed;
        }
        if let Some(permit) = self.finish.take() {
            permit.send(SessionCommand::FinishTransfer {
                slot: Arc::clone(&self.slot),
                endpoint_id: self.endpoint_id,
            });
        }
    }
}

pub async fn transfer(
    source: mpsc::Sender<SessionCommand>,
    target: mpsc::Sender<SessionCommand>,
    endpoint_id: EndpointId,
) -> anyhow::Result<()> {
    static ADMISSION: Semaphore = Semaphore::const_new(64);
    let admission = ADMISSION
        .try_acquire()
        .map_err(|_| anyhow::anyhow!("TRANSFER_BUSY"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    // Before removing anything, reserve source rollback, extraction and target
    // delivery capacity. A timeout here leaves the source endpoint untouched.
    let finish = tokio::time::timeout_at(deadline, source.clone().reserve_owned()).await;
    let finish = finish.map_err(|_| anyhow::anyhow!("TRANSFER_BUSY"))??;
    let extract = tokio::time::timeout_at(deadline, source.reserve_owned()).await;
    let extract = extract.map_err(|_| anyhow::anyhow!("TRANSFER_BUSY"))??;
    let insert = tokio::time::timeout_at(deadline, target.reserve_owned()).await;
    let insert = insert.map_err(|_| anyhow::anyhow!("TRANSFER_BUSY"))??;
    let slot = Arc::new(Mutex::new(TransferState::default()));
    let guard = FinishGuard {
        slot: Arc::clone(&slot),
        endpoint_id,
        finish: Some(finish),
    };
    // This operation outlives a disconnected controller, but has a finite
    // deadline. The slot serializes target commit against timeout rollback.
    let operation = tokio::spawn(async move {
        let _admission = admission;
        let _finish = guard;
        let (reply, receiver) = oneshot::channel();
        extract.send(SessionCommand::PrepareTransfer {
            slot: Arc::clone(&slot),
            endpoint_id,
            reply,
        });
        let prepared = tokio::time::timeout_at(deadline, receiver).await;
        prepared.map_err(|_| anyhow::anyhow!("TRANSFER_TIMEOUT"))???;
        let (reply, receiver) = oneshot::channel();
        insert.send(SessionCommand::CommitTransfer {
            slot: Arc::clone(&slot),
            reply,
        });
        let committed = tokio::time::timeout_at(deadline, receiver).await;
        // A reply may be lost after insertion; committed ownership is authoritative.
        let mut state = slot.lock().unwrap_or_else(|e| e.into_inner());
        if state.committed {
            return Ok(());
        }
        state.cancelled = true;
        drop(state);
        committed.map_err(|_| anyhow::anyhow!("TRANSFER_TIMEOUT"))???;
        anyhow::bail!("TRANSFER_FAILED")
    });
    let result = operation.await;
    result?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn full_destination_never_extracts_source() {
        let (source, mut source_rx) = mpsc::channel(4);
        let (target, _target_rx) = mpsc::channel(1);
        target.try_send(SessionCommand::Detach).unwrap();
        let result = transfer(source, target, EndpointId::new_v4()).await;
        assert!(result.is_err());
        assert!(source_rx.try_recv().is_err());
    }
    #[tokio::test(start_paused = true)]
    async fn missing_commit_reply_rolls_back_uncommitted_ownership() {
        let (source, mut source_rx) = mpsc::channel(4);
        let (target, mut target_rx) = mpsc::channel(1);
        let operation = tokio::spawn(transfer(source, target, EndpointId::new_v4()));
        let prepared = source_rx.recv().await;
        let SessionCommand::PrepareTransfer { reply, slot, .. } = prepared.unwrap() else {
            panic!("prepare expected")
        };
        reply.send(Ok(())).unwrap();
        let committed = target_rx.recv().await;
        let SessionCommand::CommitTransfer { reply, .. } = committed.unwrap() else {
            panic!("commit expected")
        };
        // Hold the reply beyond the transaction deadline, then deliver it late.
        tokio::time::advance(Duration::from_secs(11)).await;
        let result = operation.await;
        assert!(result.unwrap().is_err());
        let finished = source_rx.recv().await;
        assert!(matches!(
            finished,
            Some(SessionCommand::FinishTransfer { .. })
        ));
        assert!(slot.lock().unwrap().cancelled);
        assert!(!slot.lock().unwrap().committed);
        assert!(reply.send(Ok(())).is_err());
    }
    #[tokio::test]
    async fn committed_ownership_survives_lost_reply_and_controller() {
        let (source, mut source_rx) = mpsc::channel(4);
        let (target, mut target_rx) = mpsc::channel(1);
        let operation = tokio::spawn(transfer(source, target, EndpointId::new_v4()));
        let prepared = source_rx.recv().await;
        let SessionCommand::PrepareTransfer { reply, .. } = prepared.unwrap() else {
            panic!("prepare expected")
        };
        reply.send(Ok(())).unwrap();
        let committed = target_rx.recv().await;
        let SessionCommand::CommitTransfer { reply, slot } = committed.unwrap() else {
            panic!("commit expected")
        };
        slot.lock().unwrap().committed = true;
        operation.abort();
        drop(reply);
        let finished = source_rx.recv().await;
        assert!(matches!(
            finished,
            Some(SessionCommand::FinishTransfer { .. })
        ));
        assert!(!slot.lock().unwrap().cancelled);
        assert!(slot.lock().unwrap().committed);
    }
}

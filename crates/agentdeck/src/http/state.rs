use std::sync::Arc;

use serde::Serialize;
use tokio::sync::watch;

pub trait StatePort: Send + Sync + 'static {
    fn current(&self) -> Arc<[u8]>;
    fn subscribe(&self) -> watch::Receiver<Arc<[u8]>>;
}

#[derive(Debug, thiserror::Error)]
pub enum StatePublishError {
    #[error("state could not be serialized: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("state JSON must use one-line framing")]
    InvalidFraming,
}

/// Latest-only canonical state channel shared by snapshots and SSE clients.
pub struct StateHub {
    sender: watch::Sender<Arc<[u8]>>,
}

impl StateHub {
    pub fn new<T: Serialize>(initial: &T) -> Result<Self, StatePublishError> {
        let bytes = Arc::<[u8]>::from(serde_json::to_vec(initial)?);
        let (sender, _receiver) = watch::channel(bytes);
        Ok(Self { sender })
    }

    /// Serialize once and notify only when the exact canonical bytes changed.
    pub fn publish<T: Serialize>(&self, next: &T) -> Result<bool, StatePublishError> {
        let bytes = Arc::<[u8]>::from(serde_json::to_vec(next)?);
        if *self.sender.borrow() == bytes {
            return Ok(false);
        }
        self.sender.send_replace(bytes);
        Ok(true)
    }

    pub fn publish_bytes(&self, bytes: Vec<u8>) -> Result<bool, StatePublishError> {
        let _: serde_json::Value = serde_json::from_slice(&bytes)?;
        if bytes.contains(&b'\n') || bytes.contains(&b'\r') {
            return Err(StatePublishError::InvalidFraming);
        }
        let bytes = Arc::<[u8]>::from(bytes);
        if *self.sender.borrow() == bytes {
            return Ok(false);
        }
        self.sender.send_replace(bytes);
        Ok(true)
    }
}

impl StatePort for StateHub {
    fn current(&self) -> Arc<[u8]> {
        self.sender.borrow().clone()
    }

    fn subscribe(&self) -> watch::Receiver<Arc<[u8]>> {
        self.sender.subscribe()
    }
}

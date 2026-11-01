use serde::Serialize;
use tokio::sync::broadcast;

/// Events published by the kernel and consumed by the API's SSE endpoint.
///
/// Events are intentionally small and serializable; the UI treats them as
/// invalidation hints and refetches the authoritative resource.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    ServerStarted {
        version: String,
    },
    JobQueued {
        job_id: String,
        kind: String,
    },
    JobStarted {
        job_id: String,
        kind: String,
    },
    JobProgress {
        job_id: String,
        progress: f64,
        message: Option<String>,
    },
    JobFinished {
        job_id: String,
        kind: String,
    },
    JobFailed {
        job_id: String,
        kind: String,
        error: String,
    },
    JobCanceled {
        job_id: String,
        kind: String,
    },
}

/// In-process broadcast bus.
///
/// Slow subscribers (e.g. a stalled SSE client) do not block publishers:
/// `broadcast::send` drops the event for a lagging receiver. That is the right
/// failure mode for UI invalidation hints.
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(16));
        Self { sender }
    }

    pub fn publish(&self, event: Event) {
        // No receivers is a normal condition (engine startup, CLI use).
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_does_not_require_receivers() {
        let bus = EventBus::new(16);
        bus.publish(Event::ServerStarted {
            version: "test".into(),
        });
    }

    #[tokio::test]
    async fn subscriber_receives_event() {
        let bus = EventBus::new(16);
        let mut receiver = bus.subscribe();
        bus.publish(Event::ServerStarted {
            version: "test".into(),
        });
        let event = receiver.recv().await.unwrap();
        assert!(matches!(event, Event::ServerStarted { .. }));
    }
}

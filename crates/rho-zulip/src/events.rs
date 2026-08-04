use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;

pub enum SessionEvent {
    Connected(Box<crate::types::RegisterResponse>),
    Events(Vec<crate::types::Event>),
    /// Transport failure; the loop keeps retrying with backoff.
    Disconnected(String),
}

/// Keeping queue recovery here makes the UI's receiver a pure state consumer:
/// it never has to distinguish a routine queue rotation from a reconnect.
pub async fn run(client: Arc<crate::api::Client>, sink: UnboundedSender<SessionEvent>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if sink.is_closed() {
            return;
        }
        let registration = match client.register().await {
            Ok(response) => response,
            Err(error) => {
                if sink
                    .unbounded_send(SessionEvent::Disconnected(error.to_string()))
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        let queue_id = registration.queue_id.clone();
        let mut last_event_id = registration.last_event_id;
        if sink
            .unbounded_send(SessionEvent::Connected(Box::new(registration)))
            .is_err()
        {
            return;
        }
        backoff = Duration::from_secs(1);

        loop {
            if sink.is_closed() {
                return;
            }
            match client.get_events(&queue_id, last_event_id).await {
                Ok(batch) if batch.queue_expired => break,
                Ok(batch) => {
                    last_event_id = batch.last_event_id;
                    if !batch.events.is_empty()
                        && sink
                            .unbounded_send(SessionEvent::Events(batch.events))
                            .is_err()
                    {
                        return;
                    }
                    backoff = Duration::from_secs(1);
                }
                Err(error) => {
                    if sink
                        .unbounded_send(SessionEvent::Disconnected(error.to_string()))
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }
}

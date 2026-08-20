// Central event bus: every module broadcasts `(topic, data)` frames the same
// way daemon/src/hub.js's `emit` callback did, and the WS hub (not yet
// ported) subscribes and fans them out to connected clients.

use serde_json::Value;
use tokio::sync::broadcast;

#[derive(Clone, Debug)]
pub struct Event {
    pub topic: String,
    pub data: Value,
}

pub type Emit = broadcast::Sender<Event>;

pub fn new_bus() -> Emit {
    let (tx, _rx) = broadcast::channel(1024);
    tx
}

pub fn emit(bus: &Emit, topic: &str, data: Value) {
    // No receivers is not an error — nothing is connected yet, or ever.
    let _ = bus.send(Event {
        topic: topic.to_string(),
        data,
    });
}

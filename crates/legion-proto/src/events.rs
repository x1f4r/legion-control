//! Live event delivery for running calls.

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::EventKind;

/// One event emitted by a running call: its kind, its content, and the machine that produced it
/// when the call was forwarded or fanned out.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// What kind of event it is.
    pub kind: EventKind,
    /// Its content.
    pub data: Value,
    /// The machine that produced the event, when set by routing.
    pub machine: Option<String>,
}

/// Where a running call's `event` notifications go: to the rpc client as notifications carrying
/// the call's id, to stderr on a command line, nowhere when nobody listens.
#[derive(Clone, Default)]
pub struct EventSink {
    deliver: Option<Arc<dyn Fn(Event) + Send + Sync>>,
    machine: Option<Arc<str>>,
}

impl EventSink {
    /// A sink that hands each event's kind and content to `deliver`.
    pub fn new(deliver: impl Fn(EventKind, Value) + Send + Sync + 'static) -> EventSink {
        EventSink {
            deliver: Some(Arc::new(move |event| deliver(event.kind, event.data))),
            machine: None,
        }
    }

    /// A sink that receives the full [`Event`], including the machine that produced it.
    pub fn with_events(deliver: impl Fn(Event) + Send + Sync + 'static) -> EventSink {
        EventSink {
            deliver: Some(Arc::new(deliver)),
            machine: None,
        }
    }

    /// A sink that drops every event.
    pub fn discard() -> EventSink {
        EventSink::default()
    }

    /// Returns a sink that stamps emitted events with `machine`, overwriting any label the emitter
    /// supplied so a remote cannot forge another machine's name.
    #[must_use]
    pub fn for_machine(&self, machine: impl Into<Arc<str>>) -> EventSink {
        EventSink {
            deliver: self.deliver.clone(),
            machine: Some(machine.into()),
        }
    }

    /// Delivers one event from this node.
    pub fn emit(&self, kind: EventKind, data: Value) {
        self.emit_event(Event {
            kind,
            data,
            machine: None,
        });
    }

    /// Delivers one [`Event`], overwriting its `machine` with the machine bound to this sink when
    /// one is set.
    pub fn emit_event(&self, mut event: Event) {
        if let Some(deliver) = &self.deliver {
            if let Some(machine) = &self.machine {
                event.machine = Some(machine.to_string());
            }
            deliver(event);
        }
    }
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.deliver, &self.machine) {
            (Some(_), Some(machine)) => write!(f, "EventSink({machine})"),
            (Some(_), None) => f.write_str("EventSink"),
            (None, _) => f.write_str("EventSink(discard)"),
        }
    }
}

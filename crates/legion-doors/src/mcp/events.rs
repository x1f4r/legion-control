//! A running call's events, forwarded as the notifications the client asked for: progress when the
//! request carried a progress token, log messages when the client set a log level, nothing
//! otherwise.
//!
//! Log messages are deprecated by the newest protocol revision but are what clients speaking the
//! older one listen to, so they are sent while a client asks for them.
#![allow(
    deprecated,
    reason = "logging is deprecated in the newest MCP revision, and still asked for by the older one"
)]

use std::future::Future;

use legion_core::proto::EventKind;
use legion_core::{Event, EventSink};
use rmcp::model::{
    LoggingLevel, LoggingMessageNotificationParam, ProgressNotificationParam, ProgressToken,
    ServerCapabilities,
};
use rmcp::service::RequestContext;
use rmcp::{Peer, RoleServer};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Events waiting to be sent. A tool producing output faster than the client reads it loses some
/// of it rather than the door's memory; the result still says everything the call did.
const QUEUE: usize = 256;

/// The longest text a progress message carries, so one long line of output cannot fill a client's
/// window.
const LONGEST_MESSAGE: usize = 1000;

/// What the door offers: tools (with `listChanged` in full mode, where extensions can be added,
/// updated or removed live), and log messages for a client that asks for them. Compact mode always
/// serves the same three meta-tools, so its list never changes.
pub(crate) fn capabilities(list_changed: bool) -> ServerCapabilities {
    let builder = ServerCapabilities::builder()
        .enable_logging()
        .enable_tools();
    if list_changed {
        builder.enable_tool_list_changed().build()
    } else {
        builder.build()
    }
}

/// The log level a client asked for, kept as its rank so the deprecated type stays in this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Level(u8);

impl Level {
    /// The rank of a level the client sent.
    pub(crate) fn of(level: LoggingLevel) -> Level {
        Level(match level {
            LoggingLevel::Debug => 0,
            LoggingLevel::Info => 1,
            LoggingLevel::Notice => 2,
            LoggingLevel::Warning => 3,
            LoggingLevel::Error => 4,
            LoggingLevel::Critical => 5,
            LoggingLevel::Alert => 6,
            LoggingLevel::Emergency => 7,
        })
    }
}

/// The log level an event is sent at: what a tool says and the output of what it runs are
/// information, the ticks of its progress are detail.
fn level_of(kind: EventKind) -> LoggingLevel {
    match kind {
        EventKind::Progress => LoggingLevel::Debug,
        EventKind::Stdout | EventKind::Stderr | EventKind::Log => LoggingLevel::Info,
    }
}

/// Sends one call's events to the client.
pub(crate) struct Forwarder {
    peer: Peer<RoleServer>,
    tool: String,
    progress: Option<ProgressToken>,
    sent: f64,
    level: Option<Level>,
}

impl Forwarder {
    /// The forwarder for a call to `tool` made by this request, or `None` when the client asked
    /// for neither progress nor log messages. `session_level` is what `logging/setLevel` set; a
    /// level in the request's own `_meta` takes its place.
    pub(crate) fn new(
        context: &RequestContext<RoleServer>,
        tool: &str,
        session_level: Option<Level>,
    ) -> Option<Forwarder> {
        let progress = context.meta.get_progress_token();
        let level = context.meta.log_level().map(Level::of).or(session_level);
        if progress.is_none() && level.is_none() {
            return None;
        }
        Some(Forwarder {
            peer: context.peer.clone(),
            tool: tool.to_owned(),
            progress,
            sent: 0.0,
            level,
        })
    }

    /// A sink for the call's events and the queue it fills.
    pub(crate) fn sink() -> (EventSink, mpsc::Receiver<Event>) {
        let (queue, events) = mpsc::channel(QUEUE);
        let sink = EventSink::with_events(move |event| {
            let _ = queue.try_send(event);
        });
        (sink, events)
    }

    /// Forwards events while `work` runs, then the ones it left queued, and returns what `work`
    /// returned. The queued ones go first so no notification follows the result it belongs to.
    pub(crate) async fn forward_during<T>(
        mut self,
        mut events: mpsc::Receiver<Event>,
        work: impl Future<Output = T>,
    ) -> T {
        tokio::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => {
                    while let Ok(event) = events.try_recv() {
                        self.send(event).await;
                    }
                    return result;
                }
                Some(event) = events.recv() => self.send(event).await,
            }
        }
    }

    /// Sends one event as each notification the client asked for. A client that has gone away
    /// has nobody to tell, so a failed send is dropped.
    async fn send(&mut self, event: Event) {
        if let Some(token) = &self.progress {
            // Progress has no total: each event is one step further, which is all a client needs
            // to know the call is alive and what it is doing.
            self.sent += 1.0;
            let text = match &event.machine {
                Some(machine) => format!("[{machine}] {}", message(event.kind, &event.data)),
                None => message(event.kind, &event.data),
            };
            let param = ProgressNotificationParam::new(token.clone(), self.sent).with_message(text);
            let _ = self.peer.notify_progress(param).await;
        }
        let level = level_of(event.kind);
        if self.level.is_some_and(|wanted| Level::of(level) >= wanted) {
            let payload = match event.machine {
                Some(machine) => json!({
                    "kind": event.kind.as_str(),
                    "data": event.data,
                    "machine": machine,
                }),
                None => json!({"kind": event.kind.as_str(), "data": event.data}),
            };
            let mut param = LoggingMessageNotificationParam::new(level, payload);
            param.logger = Some(self.tool.clone());
            let _ = self.peer.notify_logging_message(param).await;
        }
    }
}

/// One event as a line of text for a progress message.
fn message(kind: EventKind, data: &Value) -> String {
    let text = match data {
        Value::String(text) => text.trim_end().to_owned(),
        other => other.to_string(),
    };
    let text = match text.char_indices().nth(LONGEST_MESSAGE) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text,
    };
    match kind {
        EventKind::Stdout | EventKind::Stderr => format!("{}: {text}", kind.as_str()),
        EventKind::Progress | EventKind::Log => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_ordered_by_severity() {
        assert!(Level::of(LoggingLevel::Debug) < Level::of(LoggingLevel::Info));
        assert!(Level::of(LoggingLevel::Warning) < Level::of(LoggingLevel::Emergency));
        assert_eq!(Level::of(level_of(EventKind::Progress)), Level(0));
        assert_eq!(Level::of(level_of(EventKind::Stdout)), Level(1));
    }

    #[test]
    fn a_message_is_one_bounded_line_saying_where_output_came_from() {
        assert_eq!(
            message(EventKind::Stdout, &json!("hello\n")),
            "stdout: hello"
        );
        assert_eq!(
            message(EventKind::Progress, &json!({"waited": 0.5})),
            "{\"waited\":0.5}"
        );
        assert_eq!(message(EventKind::Log, &json!("note")), "note");
        let long = message(EventKind::Log, &json!("é".repeat(5000)));
        assert_eq!(long.chars().count(), LONGEST_MESSAGE + 1);
        assert!(long.ends_with('…'));
    }
}

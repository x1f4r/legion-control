//! `legion cp`: `fs.transfer` from a command line, its progress kept to one line of a terminal.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use legion_core::proto::EventKind;
use legion_core::{Call, CancellationToken, Door, EventSink, Node, Router};
use serde_json::{Map, Value};

use crate::args::CpCommand;
use crate::{err, print_outcome, render, run_call};

/// Runs the transfer and prints its result as `legion fs.transfer …` would.
pub async fn run(
    node: &Node,
    router: &dyn Router,
    command: CpCommand,
    shutdown: CancellationToken,
) -> ExitCode {
    let mut args = Map::new();
    args.insert("from".to_owned(), Value::String(command.from));
    args.insert("to".to_owned(), Value::String(command.to));
    args.insert("delete".to_owned(), Value::Bool(command.delete));
    args.insert("checksum".to_owned(), Value::Bool(command.checksum));
    args.insert(
        "exclude".to_owned(),
        Value::Array(command.exclude.into_iter().map(Value::String).collect()),
    );
    args.insert("dry_run".to_owned(), Value::Bool(command.dry_run));
    let mut call = Call::new(Door::Cli, "fs.transfer", Value::Object(args));
    call.as_principal = command.as_principal;
    let line = Arc::new(ProgressLine::default());
    if io::stderr().is_terminal() {
        let shown = Arc::clone(&line);
        call.events = EventSink::with_events(move |event| {
            if event.kind == EventKind::Progress {
                shown.show(&describe(&event.data));
            } else {
                shown.clear();
                err(&render::event(&event, false));
            }
        });
    }
    let outcome = run_call(node, router, call, None, shutdown).await;
    line.clear();
    print_outcome(outcome, command.json)
}

/// The one line progress is written over, and how long it was.
#[derive(Default)]
struct ProgressLine {
    width: Mutex<usize>,
}

impl ProgressLine {
    fn show(&self, text: &str) {
        let mut width = self
            .width
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let length = text.chars().count();
        let pad = " ".repeat(width.saturating_sub(length));
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "\r{text}{pad}");
        let _ = stderr.flush();
        *width = length;
    }

    fn clear(&self) {
        let mut width = self
            .width
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *width > 0 {
            let mut stderr = io::stderr().lock();
            let _ = write!(stderr, "\r{}\r", " ".repeat(*width));
            let _ = stderr.flush();
            *width = 0;
        }
    }
}

/// A progress event as a person reads it: `42% 1.2 MiB of 2.9 MiB, 3 of 10 files`.
fn describe(data: &Value) -> String {
    let field = |name: &str| data.get(name).and_then(Value::as_u64).unwrap_or(0);
    let (bytes, total) = (field("bytes"), field("bytes_total"));
    let (files, files_total) = (field("files"), field("files_total"));
    let mut text = String::new();
    if total > 0 {
        let percent = (u128::from(bytes) * 100 / u128::from(total)).min(100);
        text.push_str(&format!("{percent:>3}% "));
    }
    text.push_str(&format!("{} of {}", size(bytes), size(total)));
    if files_total > 1 {
        text.push_str(&format!(", {files} of {files_total} files"));
    }
    text
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn progress_reads_as_a_share_of_the_whole() {
        assert_eq!(
            describe(&json!({"bytes": 1536, "bytes_total": 3072, "files": 1, "files_total": 4})),
            " 50% 1.5 KiB of 3.0 KiB, 1 of 4 files"
        );
        assert_eq!(
            describe(&json!({"bytes": 10, "bytes_total": 0, "files": 0, "files_total": 1})),
            "10 B of 0 B"
        );
        assert_eq!(size(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}

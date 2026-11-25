#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

//! Ticket-authenticated, single-range HTTP file content serving for Legion.
//!
//! Browsers and web views cannot attach `Authorization` headers to `<img>`, `<video>`, `<audio>`,
//! or download links, and file bytes must never execute scripts in the interface's origin. This
//! crate holds the shared machinery mounted by both the HTTP door (`legion-doors`) and the native
//! app:
//!
//! - [`TicketTable`]: 256-bit random tickets bound to one holder (token or pairing), principal,
//!   machine, and path, with 15-minute sliding and 6-hour absolute expiry, a per-holder cap, and
//!   holder revocation.
//! - [`handle_request`]: `GET` and `HEAD` with RFC 9110 single-range support (`Range`, `If-Range`,
//!   strong `ETag` from size and modification time, `206 Partial Content`, `416 Range Not
//!   Satisfiable`, `?download=1`), fixed defensive headers, a strict `Content-Type` allow-list,
//!   and streaming with backpressure over [`ContentSource`].

mod body;
mod media;
mod range;
mod serve;
mod tickets;

pub use body::{BoxAsyncRead, ContentBody};
pub use media::{
    CONTENT_SANDBOX_CSP, ContentPolicy, classify_filename, content_disposition_attachment,
};
pub use range::{ContentTime, ParsedRange, RangeDecision, ResolvedRange, etag, parse_range};
pub use serve::{
    ContentSource, CorpPolicy, OpenRangeError, OpenedRange, ServeOptions, handle_request,
    status_for_error,
};
pub use tickets::{
    ABSOLUTE_TTL, DEFAULT_MAX_PER_HOLDER, DEFAULT_MAX_TOTAL, IssuedTicket, SLIDING_TTL, Ticket,
    TicketConfig, TicketRequest, TicketTable, decode_path_segment, encode_path_segment,
    extract_file_name,
};

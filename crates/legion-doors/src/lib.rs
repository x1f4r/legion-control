//! The doors programs come in through, as libraries over a [`Node`](legion_core::Node) and a
//! [`Router`](legion_core::Router).
//!
//! A door only translates: it turns what a client sends into a [`Call`](legion_core::Call), hands
//! it to the router, and turns the answer back into what the client expects. Policy, auditing and
//! the tools themselves stay in the node, so every door enforces the same rules. The `legion`
//! binary wires a door to stdin and stdout or to a socket and nothing more, which keeps each door
//! testable over an in-memory pipe.

pub mod mcp;

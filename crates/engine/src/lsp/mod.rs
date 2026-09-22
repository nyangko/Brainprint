//! Backend-neutral LSP plumbing.
//!
//! Two pieces that every LSP-speaking semantic backend needs and that
//! none of them may own privately:
//!
//! * [`jsonrpc`] -- the `Content-Length` framed JSON-RPC connection
//!   over a child process's stdio. It knows nothing about any language,
//!   any server, or even LSP itself; it moves frames and matches ids.
//! * [`coordinates`] -- the line/character ↔ byte mapping. LSP counts a
//!   `character` in code units of the *negotiated* encoding, Brainprint
//!   counts bytes, and the translation between them has to be exact or
//!   a `SourceSpan` is a lie.
//!
//! This module exists because #19 task 10 added a second LSP backend.
//! Duplicating either piece would have meant two framings to keep in
//! step and two chances to clamp a position. What stays *out* of here
//! is anything a particular server means: method names, capability
//! classes, project models and environment policy live with their
//! backend, not here.

pub mod coordinates;
pub mod jsonrpc;

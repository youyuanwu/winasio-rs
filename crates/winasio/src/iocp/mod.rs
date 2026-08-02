// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Runtime-agnostic infrastructure for Windows overlapped I/O.
//!
//! This module turns any Windows API that takes an `OVERLAPPED` into something
//! awaitable, without depending on a particular async runtime and without
//! requiring changes to this crate for each new API.
//!
//! # Defining an operation
//!
//! Implement [`OpCode`]. The operation owns whatever state Windows touches —
//! a byte buffer, a caller-allocated structure, or both — and gets it back when
//! the operation completes.
//!
//! # Ownership and cancellation
//!
//! Submitting an operation transfers its state to the driver. If the awaiting
//! future is dropped before completion, cancellation is requested immediately,
//! but **the state is not returned to the caller** and its memory is retained
//! until Windows delivers the completion. This is inherent to completion-based
//! I/O: the kernel may still be writing into the buffer, so releasing it earlier
//! would be a use-after-free.

mod buf;
mod op;
mod raw;

pub use buf::{BufResult, IoBuf, IoBufMut};
pub use op::{win32_result, IntoInner, OpCode, OpType};

#[cfg(any(test, feature = "test-util"))]
pub use raw::live_operations;

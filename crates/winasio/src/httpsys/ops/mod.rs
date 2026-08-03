// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Operations on a request queue.

pub(crate) mod cancel;
pub(crate) mod receive;

use windows::Win32::Foundation::HANDLE;

/// A queue handle usable from any thread.
///
/// `HANDLE` is a raw pointer and so not `Send`; a request-queue handle is
/// nevertheless thread-agnostic.
#[derive(Clone, Copy, Debug)]
pub(crate) struct QueueHandle(pub(crate) HANDLE);

// SAFETY: see above.
unsafe impl Send for QueueHandle {}
unsafe impl Sync for QueueHandle {}

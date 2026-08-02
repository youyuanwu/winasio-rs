// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Reference operations shipped with the crate.

pub mod event;
pub mod file;

pub use event::WaitForHandle;
pub use file::{ReadAt, SendHandle, WriteAt};

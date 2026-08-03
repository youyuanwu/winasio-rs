// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! File open options.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_ALWAYS, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_CREATION_DISPOSITION,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
    OPEN_EXISTING, TRUNCATE_EXISTING,
};

use crate::iocp::{Handle, Registrar};

use super::error::SetupError;
use super::File;

/// Builder for opening an overlapped file.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
    share_mode: FILE_SHARE_MODE,
    flags_and_attributes: FILE_FLAGS_AND_ATTRIBUTES,
}

impl OpenOptions {
    /// Create a builder with no access bits selected.
    pub fn new() -> Self {
        OpenOptions {
            read: false,
            write: false,
            create: false,
            create_new: false,
            truncate: false,
            share_mode: FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            flags_and_attributes: FILE_ATTRIBUTE_NORMAL,
        }
    }

    /// Enable or disable read access.
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    /// Enable or disable write access.
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    /// Create the file if it is missing.
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    /// Require creating a new file and fail if it already exists.
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    /// Truncate the file when it is opened.
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    /// Set the Windows share mode.
    pub fn share_mode(&mut self, share_mode: FILE_SHARE_MODE) -> &mut Self {
        self.share_mode = share_mode;
        self
    }

    /// Add custom Windows flags and attributes.
    ///
    /// `FILE_FLAG_OVERLAPPED` is always added by [`OpenOptions::open`] even if
    /// it is not present here.
    pub fn custom_flags_and_attributes(
        &mut self,
        flags_and_attributes: FILE_FLAGS_AND_ATTRIBUTES,
    ) -> &mut Self {
        self.flags_and_attributes = flags_and_attributes;
        self
    }

    /// Open and register a file with `registrar`.
    pub fn open<R: Registrar>(
        &self,
        registrar: &R,
        path: impl AsRef<Path>,
    ) -> Result<File<R::Io>, SetupError> {
        let wide = wide_null(path.as_ref())?;
        let desired_access = self.desired_access();
        let creation = self.creation_disposition();
        let flags = self.flags_and_attributes | FILE_FLAG_OVERLAPPED;

        // SAFETY: the path is explicitly NUL-terminated, and all other
        // arguments are values owned by this builder.
        let raw = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                desired_access,
                self.share_mode,
                None,
                creation,
                flags,
                None,
            )
        }
        .map_err(SetupError::from_windows)?;

        // SAFETY: `CreateFileW` returned a newly owned handle, and ownership of
        // closing it transfers into `Handle`.
        let handle = unsafe { Handle::from_raw(raw) };
        match registrar.register(handle.raw()) {
            Ok(submitter) => Ok(File::from_parts(handle, submitter)),
            Err(e) => {
                drop(handle);
                Err(SetupError::from(e))
            }
        }
    }

    fn desired_access(&self) -> u32 {
        let mut access = 0;
        if self.read {
            access |= FILE_GENERIC_READ.0;
        }
        if self.write {
            access |= FILE_GENERIC_WRITE.0 | GENERIC_WRITE.0;
        }
        access
    }

    fn creation_disposition(&self) -> FILE_CREATION_DISPOSITION {
        if self.create_new {
            CREATE_NEW
        } else if self.create && self.truncate {
            CREATE_ALWAYS
        } else if self.create {
            OPEN_ALWAYS
        } else if self.truncate {
            TRUNCATE_EXISTING
        } else {
            OPEN_EXISTING
        }
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

fn wide_null(path: &Path) -> Result<Vec<u16>, SetupError> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(SetupError::InvalidName);
    }
    wide.push(0);
    Ok(wide)
}

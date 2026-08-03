// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Named-pipe name validation.

use windows::core::HSTRING;

use crate::fs::SetupError;

const LOCAL_PIPE_PREFIX: &str = r"\\.\pipe\";

/// Maximum accepted bare pipe-name component length.
pub const MAX_NAME_COMPONENT_LEN: usize = 256 - LOCAL_PIPE_PREFIX.len();

pub(crate) fn local_pipe_path(name: &str) -> Result<HSTRING, SetupError> {
    validate_bare_name(name)?;
    Ok(HSTRING::from(format!("{LOCAL_PIPE_PREFIX}{name}")))
}

fn validate_bare_name(name: &str) -> Result<(), SetupError> {
    if name.is_empty()
        || name.len() > MAX_NAME_COMPONENT_LEN
        || name.contains(['\\', '/'])
        || name.as_bytes().contains(&0)
    {
        Err(SetupError::InvalidName)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_bare_names() {
        for name in ["", "has\\slash", "has/slash", "nul\0inside"] {
            assert!(matches!(
                local_pipe_path(name),
                Err(SetupError::InvalidName)
            ));
        }
        let too_long = "x".repeat(MAX_NAME_COMPONENT_LEN + 1);
        assert!(matches!(
            local_pipe_path(&too_long),
            Err(SetupError::InvalidName)
        ));
    }
}

// SPDX-License-Identifier: MIT
//! Windows resources linked into Kanade's binaries.
//!
//! Build scripts run this on the build host. It needs neither a resource
//! compiler nor the Windows SDK, so a Windows target can be checked from any
//! host. Nothing here is loaded into an input method host.

pub mod brand_icon;
pub mod icon;
pub mod res;

/// Why an icon or a resource file could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the icon template is not a readable PNG: {0}")]
    Decode(#[from] png::DecodingError),
    #[error("the icon template {0}")]
    Template(&'static str),
    #[error("an icon {0}")]
    Icon(&'static str),
}

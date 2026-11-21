// SPDX-License-Identifier: MIT
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#[cfg(windows)]
mod native;
#[cfg(windows)]
#[doc(hidden)]
pub use native::BRAND_ICON_INDEX;

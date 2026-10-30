//! Starts resolver publication and guest authority delivery under shared task ownership.

#[cfg(target_os = "macos")]
mod guest;
#[cfg(target_os = "macos")]
mod publication;
#[cfg(target_os = "macos")]
pub use publication::start;

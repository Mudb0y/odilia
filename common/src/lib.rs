#![deny(
	clippy::all,
	clippy::pedantic,
//	clippy::cargo,
	clippy::map_unwrap_or,
	clippy::unwrap_used,
	unsafe_code
)]

use zbus::{names::UniqueName, zvariant::ObjectPath};

pub mod elements;
pub mod errors;
pub mod events;
pub mod modes;
pub mod result;
pub mod settings;
pub mod types;

pub type Accessible = (UniqueName<'static>, ObjectPath<'static>);
pub use result::OdiliaResult as Result;

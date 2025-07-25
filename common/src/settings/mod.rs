pub mod input;
pub mod log;
pub mod speech;

pub use input::{InputMethod, InputSettings};
pub use log::LogSettings;
use serde::{Deserialize, Serialize};
pub use speech::SpeechSettings;

///type representing a *read-only* view of the odilia screenreader configuration
/// this type should only be obtained as a result of parsing odilia's configuration files, as it containes types for each section responsible for controlling various parts of the screenreader
/// the only way this config should change is if the configuration file changes, in which case the entire view will be replaced to reflect the fact
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct ApplicationConfig {
	pub speech: SpeechSettings,
	pub log: LogSettings,
	pub input: InputSettings,
}

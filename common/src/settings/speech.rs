use serde::{Deserialize, Serialize};
///structure for all the speech related configuration options available in odilia
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[allow(clippy::module_name_repetitions)]
pub struct SpeechSettings {
	pub rate: i32,
}
impl SpeechSettings {
	pub fn new(rate: i32) -> Self {
		Self { rate }
	}
}

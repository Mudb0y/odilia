use std::{cell::Cell, fs, sync::atomic::AtomicI32};

use circular_queue::CircularQueue;
use eyre::WrapErr;
use ssip_client::{tokio::Request as SSIPRequest, MessageScope, Priority};
use tokio::sync::{mpsc::Sender, Mutex};
use zbus::{fdo::DBusProxy, names::UniqueName, zvariant::ObjectPath, MatchRule, MessageType};

use odilia_cache::{
	Cache,
  CacheItem,
};
use atspi::{
	accessible::{AccessibleProxy, ObjectPair}, accessible_ext::{AccessibleExt}, cache::CacheProxy,
	convertable::Convertable, text::Granularity,
	signify::Signified,
};
use odilia_common::{
	modes::ScreenReaderMode, settings::ApplicationConfig, types::TextSelectionArea,
	Result as OdiliaResult,
};

pub struct ScreenReaderState {
	pub atspi: atspi::Connection,
	pub dbus: DBusProxy<'static>,
	pub ssip: Sender<SSIPRequest>,
	pub config: ApplicationConfig,
	pub previous_caret_position: AtomicI32,
	pub mode: Mutex<ScreenReaderMode>,
	pub granularity: Mutex<Granularity>,
	pub accessible_history: Mutex<CircularQueue<ObjectPair>>,
	pub event_history: Mutex<CircularQueue<atspi::Event>>,
	pub cache: Cache,
}

impl ScreenReaderState {
	#[tracing::instrument]
	pub async fn new(ssip: Sender<SSIPRequest>) -> eyre::Result<ScreenReaderState> {
		let atspi = atspi::Connection::open()
			.await
			.wrap_err("Could not connect to at-spi bus")?;
		let dbus = DBusProxy::new(atspi.connection())
			.await
			.wrap_err("Failed to create org.freedesktop.DBus proxy")?;

		let mode = Mutex::new(ScreenReaderMode { name: "CommandMode".to_string() });

		tracing::debug!("Reading configuration");
		let xdg_dirs = xdg::BaseDirectories::with_prefix("odilia").expect(
            "unable to find the odilia config directory according to the xdg dirs specification",
        );
		let config_path = xdg_dirs.place_config_file("config.toml").expect(
			"unable to place configuration file. Maybe your system is readonly?",
		);
		if !config_path.exists() {
			fs::copy("config.toml", &config_path)
				.expect("Unable to copy default config file.");
		}
		let config_path = config_path.to_str().unwrap().to_owned();
		tracing::debug!(path=%config_path, "loading configuration file");
		let config = ApplicationConfig::new(&config_path)
			.wrap_err("unable to load configuration file")?;
		tracing::debug!("configuration loaded successfully");

		let previous_caret_position = AtomicI32::new(0);
		let accessible_history = Mutex::new(CircularQueue::with_capacity(16));
		let event_history = Mutex::new(CircularQueue::with_capacity(16));
		let cache = Cache::new();

		let granularity = Mutex::new(Granularity::Line);
		Ok(Self {
			atspi,
			dbus,
			ssip,
			config,
			previous_caret_position,
			mode,
			granularity,
			accessible_history,
			event_history,
			cache,
		})
	}

	// TODO: use cache; this will uplift performance MASSIVELY, also TODO: use this function instad of manually generating speech every time.
	#[allow(dead_code)]
	pub async fn generate_speech_string(
		&self,
		acc: AccessibleProxy<'_>,
		select: TextSelectionArea,
	) -> OdiliaResult<String> {
		let acc_text = acc.to_text().await?;
		let _acc_hyper = acc.to_hyperlink().await?;
		//let _full_text = acc_text.get_text_ext().await?;
		let (mut text_selection, start, end) = match select {
			TextSelectionArea::Granular(granular) => {
				acc_text.get_string_at_offset(granular.index, granular.granularity)
					.await?
			}
			TextSelectionArea::Index(indexed) => (
				acc_text.get_text(indexed.start, indexed.end).await?,
				indexed.start,
				indexed.end,
			),
		};
		// TODO: Use streaming filters, or create custom function
		let children = acc.get_children_ext().await?;
		let mut children_in_range = Vec::new();
		for child in children {
			let child_hyper = child.to_hyperlink().await?;
			let index = child_hyper.start_index().await?;
			if index >= start && index <= end {
				children_in_range.push(child);
			}
		}
		for child in children_in_range {
			let child_hyper = child.to_hyperlink().await?;
			let child_start = child_hyper.start_index().await? as usize;
			let child_end = child_hyper.end_index().await? as usize;
			let child_text = format!(
				"{}, {}",
				child.name().await?,
				child.get_role_name().await?
			);
			text_selection.replace_range(
				child_start + (start as usize)..child_end + (start as usize),
				&child_text,
			);
		}
		// TODO: add logic for punctuation
		Ok(text_selection)
	}

	pub async fn register_event(&self, event: &str) -> OdiliaResult<()> {
		let match_rule = event_to_match_rule(event)?;
		self.dbus.add_match_rule(match_rule).await?;
		self.atspi.register_event(event).await?;
		Ok(())
	}

	#[allow(dead_code)]
	pub async fn deregister_event(&self, event: &str) -> OdiliaResult<()> {
		let match_rule = event_to_match_rule(event)?;
		self.dbus.remove_match_rule(match_rule).await?;
		self.atspi.deregister_event(event).await?;
		Ok(())
	}

	pub fn connection(&self) -> &zbus::Connection {
		self.atspi.connection()
	}

	pub async fn stop_speech(&self) -> bool {
		self.ssip.send(SSIPRequest::Cancel(MessageScope::All)).await.is_ok()
	}

	pub async fn close_speech(&self) -> bool {
		self.ssip.send(SSIPRequest::Quit).await.is_ok()
	}

	pub async fn say(&self, priority: Priority, text: String) -> bool {
		if self.ssip.send(SSIPRequest::SetPriority(priority)).await.is_err() {
			return false;
		}
		if self.ssip.send(SSIPRequest::Speak).await.is_err() {
			return false;
		}
		// this crashed ssip-client because the connection is automatically stopped when invalid text is sent; since the period character on a line by itself is the stop character, there's not much we can do except filter it out explicitly.
		if text == *"." {
			return false;
		}
		if self.ssip
			.send(SSIPRequest::SendLines(Vec::from([text])))
			.await
			.is_err()
		{
			return false;
		}
		true
	}

#[allow(dead_code)]
	pub async fn event_history_item(
		&self,
		index: usize
	) -> Option<atspi::Event> {
		let history = self.event_history.lock().await;
		history.iter().nth(index).cloned()
	}

	pub async fn event_history_update(
		&self,
		event: atspi::Event,
	) {
		let mut history = self.event_history.lock().await;
		history.push(event);
	}

	pub async fn history_item<'a>(
		&self,
		index: usize,
	) -> OdiliaResult<Option<CacheItem>> {
		let history = self.accessible_history.lock().await;
		if history.len() <= index {
			return Ok(None);
		}
		let object_pair = match history
				.iter()
				.nth(index)
        .to_owned() {
      Some(id) => id,
      None => {
        return Ok(None);
      },
    };
    let cache_item = self.cache.get(object_pair.1).await;
		Ok(cache_item)
	}

	/// Adds a new accessible to the history. We only store 16 previous accessibles, but theoretically, it should be lower.
	pub async fn update_accessible<T: TryInto<ObjectPair>>(&self, new_a11y: T) -> OdiliaResult<()> {
		let mut history = self.accessible_history.lock().await;
		history.push(new_a11y.try_into()?);
    Ok(())
	}
	pub async fn build_cache<'a>(
		&self,
		dest: UniqueName<'a>,
	) -> OdiliaResult<CacheProxy<'a>> {
		println!("CACHE SENDER: {}", dest);
		Ok(CacheProxy::builder(self.connection())
			.destination(dest)?
			.path(ObjectPath::from_static_str("/org/a11y/atspi/cache")?)?
			.build()
			.await?)
	}
	pub async fn new_accessible<T: Signified> (
		&self,
		event: &T,
	) -> OdiliaResult<AccessibleProxy<'_>> {
		let sender = event.sender().unwrap().unwrap().to_owned();
		let path = event.path().unwrap().to_owned();
		Ok(AccessibleProxy::builder(self.connection())
			.cache_properties(zbus::CacheProperties::No)
			.destination(sender)?
			.path(path)?
			.build()
			.await?)
	}
	pub async fn add_cache_match_rule(&self) -> OdiliaResult<()> {
		let cache_rule = MatchRule::builder()
			.msg_type(MessageType::Signal)
			.interface("org.a11y.atspi.Cache")?
			.build();
		self.dbus.add_match_rule(cache_rule).await?;
		Ok(())
	}
}
use atspi::events::GenericEvent;

/// Converts an at-spi event string ("Object:StateChanged:Focused"), into a DBus match rule ("type='signal',interface='org.a11y.atspi.Event.Object',member='StateChanged'")
fn event_to_match_rule(event: &str) -> OdiliaResult<zbus::MatchRule> {
	let mut components = event.split(':');
	let interface = components
		.next()
		.expect("Event should consist of at least 2 components separated by ':'");
	let member = components
		.next()
		.expect("Event should consist of at least 2 components separated by ':'");
	Ok(MatchRule::builder()
		.msg_type(MessageType::Signal)
		.interface(format!("org.a11y.atspi.Event.{interface}"))?
		.member(member)?
		.build())
}

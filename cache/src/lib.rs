#![deny(
	clippy::all,
	clippy::pedantic,
	clippy::cargo,
	clippy::map_unwrap_or,
	clippy::unwrap_used,
	unsafe_code
)]
#![allow(clippy::multiple_crate_versions)]

mod convertable;
pub use convertable::Convertable;
mod accessible_ext;
use std::{collections::HashMap, fmt, fmt::Debug, future::Future};
mod relation_set;
pub use relation_set::{RelationSet, Relations};
mod event_handlers;

pub use accessible_ext::AccessibleExt;
use async_channel::{Receiver, Sender};
use atspi::{
	proxy::{accessible::AccessibleProxy, text::TextProxy},
	Event, EventProperties, InterfaceSet, ObjectRef, RelationType, Role, StateSet,
};
pub use event_handlers::{
	CacheRequest, CacheResponse, Children, ConstRelationType, ControlledBy, ControllerFor,
	DescribedBy, DescriptionFor, Details, DetailsFor, EmbeddedBy, Embeds, ErrorFor,
	ErrorMessage, EventHandler, FlowsFrom, FlowsTo, Item, LabelFor, LabelledBy, MemberOf,
	NodeChildOf, NodeParentOf, Parent, ParentWindowOf, PopupFor, SubwindowOf,
};
use futures_concurrency::future::TryJoin;
use futures_lite::future::FutureExt as LiteExt;
use futures_util::future::{ok, Either, FutureExt, TryFutureExt};
use fxhash::FxBuildHasher;
use odilia_common::{
	cache::AccessiblePrimitive,
	errors::{CacheError, OdiliaError},
	result::OdiliaResult,
};
use serde::{Deserialize, Serialize};
use smol_cancellation_token::CancellationToken;
use static_assertions::assert_impl_all;
use zbus::proxy::CacheProperties;

async fn or_cancel<F>(f: F, token: &CancellationToken) -> Result<F::Output, std::io::Error>
where
	F: std::future::Future,
{
	token.cancelled()
		.map(|()| Err(std::io::ErrorKind::TimedOut.into()))
		.or(f.map(Ok))
		.await
}

use async_channel::bounded;

/// A method of interacting with the cache.
/// All requests on the cache side are processed in synchronous FIFO order.
///
/// You may clone this item for cheap in order to share across threads or tasks.
#[derive(Clone)]
pub struct CacheActor {
	send: Sender<(CacheRequest, Sender<Result<CacheResponse, OdiliaError>>)>,
}

impl CacheActor {
	#[must_use]
	pub fn new(
		send: Sender<(CacheRequest, Sender<Result<CacheResponse, OdiliaError>>)>,
	) -> Self {
		CacheActor { send }
	}
}

impl fmt::Debug for CacheActor {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		fmt.debug_struct("CacheActor").finish_non_exhaustive()
	}
}

impl From<Sender<(CacheRequest, Sender<Result<CacheResponse, OdiliaError>>)>> for CacheActor {
	fn from(send: Sender<(CacheRequest, Sender<Result<CacheResponse, OdiliaError>>)>) -> Self {
		CacheActor { send }
	}
}
impl CacheActor {
	/// Request the [`CacheRequest`] from the cache.
	///
	/// # Errors
	///
	/// The possible errors are outlined in [`CacheError`].
	///
	/// # Panics
	///
	/// If the receiver for the response is dropped.
	pub async fn request(&self, req: CacheRequest) -> Result<CacheResponse, OdiliaError> {
		let (reply, recv) = bounded(1);
		self.send
			.send((req, reply))
			.await
			.expect("Unable to send a message on the channel; this is bad!");
		recv.recv().await.expect("Unable to get response from channel!")
	}
}

pub type ActorRequest = (CacheRequest, Sender<Result<CacheResponse, OdiliaError>>);
pub type ActorSend = Sender<ActorRequest>;
pub type ActorRecv = Receiver<ActorRequest>;

pub async fn cache_handler_task<D: CacheDriver + Send>(
	recv: ActorRecv,
	shutdown: CancellationToken,
	mut cache: Cache<D>,
) {
	loop {
		let Ok(maybe_request) = or_cancel(recv.recv(), &shutdown).await else {
			tracing::info!("Shutting down cache service due to cancellation token!");
			break;
		};
		tracing::trace!("MR: {maybe_request:?}");
		let (request, response) = match maybe_request {
			Err(e) => {
				tracing::error!(error = %e, "Error receiving cache request");
				continue;
			}
			Ok(req) => req,
		};
		tracing::trace!("REQ: {request:?}");
		let maybe_cache_item = cache.request(request).await;
		match response.send(maybe_cache_item).await {
			Ok(()) => tracing::trace!("Successful sending cache item back!"),
			Err(e) => {
				tracing::error!(error = %e, "Error sending cache item back to requester!");
			}
		}
	}
}

trait AllText {
	async fn get_all_text(self) -> Result<String, zbus::Error>;
}
impl AllText for TextProxy<'_> {
	async fn get_all_text(self) -> Result<String, zbus::Error> {
		let length_of_string = self.character_count().await?;
		self.get_text(0, length_of_string).await
	}
}

pub type CacheKey = AccessiblePrimitive;
type NewCache = HashMap<CacheKey, CacheItem, FxBuildHasher>;

#[derive(Clone, Debug, Deserialize, Serialize)]
/// A struct representing an accessible. To get any information from the cache other than the stored information like role, interfaces, and states, you will need to instantiate an [`atspi::proxy::accessible::AccessibleProxy`] or other `*Proxy` type from atspi to query further info.
pub struct CacheItem {
	/// The accessible object (within the application)    (so)
	pub object: AccessiblePrimitive,
	/// The application (root object(?)      (so)
	pub app: AccessiblePrimitive,
	/// The parent object.  (so)
	pub parent: AccessiblePrimitive,
	/// The accessible index in parent.I
	pub index: Option<usize>,
	/// Child count of the accessible.I
	pub children_num: Option<usize>,
	/// The exposed interface(s) set
	pub interfaces: InterfaceSet,
	/// Accessible role. u
	pub role: Role,
	/// The states applicable to the accessible.  au
	pub states: StateSet,
	/// The children (ids) of the accessible
	pub children: Vec<AccessiblePrimitive>,
	/// The human-readable short name of the item. `None` if string is empty.
	pub name: Option<String>,
	/// The human-readable longer name (description) of the item. `None` if string is empty.
	pub description: Option<String>,
	/// The help-text of the item. `None` if string is empty.
	pub help_text: Option<String>,
	/// The actual, internal text of the item; this will be `None` if either the text interface isn't
	/// implemented, or if the response contains an empty string: "".
	pub text: Option<String>,
}

impl CacheItem {
	/// Creates a `CacheItem` from an [`atspi::Event`] type.
	/// # Errors
	/// This can fail under three possible conditions:
	///
	/// 1. We are unable to convert information from the event into an [`AccessiblePrimitive`] hashmap key. This should never happen.
	/// 2. We are unable to convert the [`AccessiblePrimitive`] to an [`atspi::proxy::accessible::AccessibleProxy`].
	/// 3. The `accessible_to_cache_item` function fails for any reason. This also shouldn't happen.
	#[tracing::instrument(level = "trace", skip_all, ret, err(level = "warn"))]
	pub async fn from_atspi_event<T: EventProperties, E: CacheDriver>(
		event: &T,
		external: &E,
	) -> OdiliaResult<Self> {
		let a11y_prim = AccessiblePrimitive::from_event(event);
		external.lookup_external(&a11y_prim).await
	}
}

/// An internal cache used within Odilia.
///
/// This contains (mostly) all accessibles in the entire accessibility tree, and
/// they are referenced by their IDs. If you are having issues with incorrect or
/// invalid accessibles trying to be accessed, this is code is probably the issue.
#[derive(Clone)]
pub struct Cache<D: CacheDriver> {
	pub tree: NewCache,
	pub driver: D,
}

assert_impl_all!(Cache<zbus::Connection>: Send);

impl<D: CacheDriver> std::fmt::Debug for Cache<D> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&format!("Cache {{ tree: ...{} nodes..., .. }}", self.tree.len()))
	}
}

pub trait CacheExt {
	/// Get a single item from the cache. This will also get the information from `DBus` if it does not
	/// exist in the cache.
	fn get_ipc(
		&self,
		id: &CacheKey,
	) -> impl Future<Output = Result<CacheItem, OdiliaError>> + Send;
	fn item_from_event<T: EventProperties + Sync>(
		&self,
		ev: &T,
	) -> impl Future<Output = OdiliaResult<CacheItem>> + Send;
}

/// A method of performing I/O side-effects outside the cache itself.
/// This is made an explicit trait such that we can either:
///
/// 1. Call out to `DBus` (production), or
/// 2. Use a fixed set of items (testing).
/// 3. Panic when called.
///
/// Feel free to implement your own at your convenience.
pub trait CacheDriver {
	/// Lookup a given [`CacheKey`] that was not found in the cache..
	fn lookup_external(
		&self,
		key: &CacheKey,
	) -> impl Future<Output = OdiliaResult<CacheItem>> + Send;
	fn lookup_relations(
		&self,
		key: &CacheKey,
		ty: RelationType,
	) -> impl Future<Output = OdiliaResult<Vec<CacheKey>>> + Send;
}

impl CacheDriver for zbus::Connection {
	#[tracing::instrument(level = "trace", ret, skip(self), fields(key.item, key.name))]
	async fn lookup_external(&self, key: &CacheKey) -> OdiliaResult<CacheItem> {
		let accessible = AccessibleProxy::builder(self)
			.destination(key.sender.clone())?
			.cache_properties(CacheProperties::No)
			.path(key.id.clone())?
			.build()
			.await?;
		accessible_to_cache_item(&accessible).await
	}
	#[tracing::instrument(level = "trace", ret, skip(self), fields(key.item, key.name))]
	async fn lookup_relations(
		&self,
		key: &CacheKey,
		ty: RelationType,
	) -> OdiliaResult<Vec<CacheKey>> {
		let accessible = AccessibleProxy::builder(self)
			.destination(key.sender.clone())?
			.cache_properties(CacheProperties::No)
			.path(key.id.clone())?
			.build()
			.await?;
		Ok(accessible
			.get_relation_set()
			.await?
			.into_iter()
			.filter_map(|(rel_ty, vec)| if rel_ty == ty { Some(vec) } else { None })
			.map(|vec| {
				vec.into_iter()
					.map(Into::<CacheKey>::into)
					.collect::<Vec<CacheKey>>()
			})
			.next()
			.unwrap_or_default())
	}
}

impl<D: CacheDriver + Send> Cache<D> {
	async fn handle_event(&mut self, ev: Event) -> Result<CacheItem, OdiliaError> {
		ev.handle_event(self).await
	}
	async fn request(&mut self, req: CacheRequest) -> Result<CacheResponse, OdiliaError> {
		tracing::trace!("REQ: {req:?}");
		match req {
			CacheRequest::Item(ref key) => {
				self.get_or_create(key)
					.map_ok(|ci| CacheResponse::Item(Item(ci)))
					.await
			}
			CacheRequest::Parent(ref key) => {
				self.get_or_create(key)
					.map_ok(|ci| CacheResponse::Parent(Parent(ci)))
					.await
			}
			CacheRequest::Children(ref key) => {
				let children_vec = self.get_or_create(key).await?.children;
				let children = self.get_or_create_all(children_vec).await?;
				Ok(CacheResponse::Children(Children(children)))
			}
			CacheRequest::Relation(ref key, ty) => {
				let rel_ids = self.driver.lookup_relations(key, ty).await?;
				let rels = self.get_or_create_all(rel_ids).await?;
				Ok(CacheResponse::Relations(Relations(ty, rels)))
			}
			CacheRequest::EventHandler(event) => self
				.handle_event(*event)
				.await
				.map(|item| CacheResponse::Item(Item(item))),
		}
	}
}

// N.B.: we are using std RwLockes internally here, within the cache hashmap
// entries. When adding async methods, take care not to hold these mutexes
// across .await points.
impl<D: CacheDriver> Cache<D> {
	/// create a new, fresh cache
	#[must_use]
	#[tracing::instrument(level = "debug", ret, skip_all)]
	pub fn new(driver: D) -> Self {
		Self { tree: HashMap::with_hasher(FxBuildHasher::default()), driver }
	}

	/// Remove a single cache item. This function can not fail.
	#[tracing::instrument(level = "trace", skip(self))]
	pub fn remove(&mut self, id: &CacheKey) -> Option<CacheItem> {
		let Some(item) = self.tree.remove(id) else {
			tracing::warn!("Attempted to remove an item that doesn't exist: {id:?}");
			return None;
		};
		Some(item)
	}

	/// Get a single item from the cache.
	///
	/// This will allow you to get the item without holding any locks to it,
	/// at the cost of (1) a clone and (2) no guarantees that the data is kept up-to-date.
	#[must_use]
	#[tracing::instrument(level = "trace", skip(self), ret)]
	pub fn get(&self, id: &CacheKey) -> Option<CacheItem> {
		let ref_item = self.tree.get(id)?;
		// clone the reference into an owned value
		Some(ref_item.clone())
	}

	/// get a many items from the cache; this only creates one read handle (note that this will copy all data you would like to access)
	#[must_use]
	#[tracing::instrument(level = "trace", skip(self), ret)]
	pub fn get_all(&self, ids: &[CacheKey]) -> Vec<Option<CacheItem>> {
		ids.iter().map(|id| self.get(id)).collect()
	}

	/// Bulk remove all ids in the cache; this only refreshes the cache after removing all items.
	#[tracing::instrument(level = "trace", ret, skip(self))]
	pub fn remove_all(&mut self, ids: &[CacheKey]) {
		for id in ids {
			self.remove(id);
		}
	}

	pub fn add(&mut self, ci: CacheItem) -> CacheItem {
		self.tree.insert(ci.object.clone(), ci.clone());
		ci
	}
	pub fn add_all(&mut self, cis: Vec<CacheItem>) -> Vec<CacheItem> {
		let clone = cis.clone();
		for ci in cis {
			self.add(ci);
		}
		clone
	}

	async fn get_or_create_all(&mut self, keys: Vec<CacheKey>) -> OdiliaResult<Vec<CacheItem>> {
		let items: Vec<CacheItem> = keys
			.iter()
			.map(|key| match self.tree.get(key) {
				Some(cache_item) => Either::Left(ok(cache_item.clone())),
				None => Either::Right(self.driver.lookup_external(key)),
			})
			.collect::<Vec<_>>()
			.try_join()
			.await?;
		Ok(self.add_all(items))
	}

	/// Modify the given item with closure [`F`] if it was already contained in the cache.
	/// Otherwise, fetch a new item over the [`CacheDriver`].
	///
	/// # Errors
	///
	/// See: [`get_or_create`]
	pub async fn modify_if_not_new<F>(
		&mut self,
		key: &AccessiblePrimitive,
		f: F,
	) -> OdiliaResult<CacheItem>
	where
		F: FnOnce(&mut CacheItem),
	{
		// if the item already exists in the cache, modify it
		if let Some(cache_item) = self.tree.get_mut(key) {
			f(cache_item);
			return Ok(cache_item.clone());
		}
		let cache_item = self.driver.lookup_external(key).await?;
		self.tree.insert(key.clone(), cache_item.clone());
		Ok(cache_item)
	}

	/// Get a single item from the cache (note that this copies some integers to a new struct).
	/// If the `CacheItem` is not found, create one, add it to the cache, and return it.
	/// # Errors
	/// The function will return an error if:
	/// 1. The `accessible` can not be turned into an `AccessiblePrimitive`. This should never happen, but is technically possible.
	/// 2. The [`Self::add`] function fails.
	/// 3. The [`accessible_to_cache_item`] function fails.
	///
	/// # Panics
	///
	/// This function technically has a `.expect()` which could panic. But we gaurs against this.
	#[tracing::instrument(level = "trace", ret, err(level = "warn"), skip(self))]
	async fn get_or_create(&mut self, key: &AccessiblePrimitive) -> OdiliaResult<CacheItem> {
		// if the item already exists in the cache, return it
		if let Some(cache_item) = self.get(key) {
			return Ok(cache_item);
		}
		let cache_item = self.driver.lookup_external(key).await?;
		self.tree.insert(key.clone(), cache_item.clone());
		Ok(cache_item)
	}

	/// Clears the cache completely.
	pub fn clear(&mut self) {
		self.tree.clear();
	}
}

/// Convert an [`atspi::proxy::accessible::AccessibleProxy`] into a [`crate::CacheItem`].
/// This runs a bunch of long-awaiting code and can take quite some time; use this sparingly.
/// This takes most properties and some function calls through the `AccessibleProxy` structure and generates a new `CacheItem`, which will be written to cache before being sent back.
///
/// # Errors
///
/// Will return an `Err(_)` variant when:
///
/// 1. The `cache` parameter does not reference an active cache once the `Weak` is upgraded to an `Option<Arc<_>>`.
/// 2. Any of the function calls on the `accessible` fail.
/// 3. Any `(String, OwnedObjectPath) -> AccessiblePrimitive` conversions fail. This *should* never happen, but technically it is possible.
#[tracing::instrument(level = "trace", ret, err(level = "warn"))]
pub async fn accessible_to_cache_item(accessible: &AccessibleProxy<'_>) -> OdiliaResult<CacheItem> {
	let props = (
		accessible.get_application(),
		accessible.parent(),
		accessible.get_index_in_parent(),
		accessible.child_count(),
		accessible.get_interfaces(),
		accessible.get_role(),
		accessible.get_state(),
		accessible.get_children(),
	)
		.try_join();
	let maps = (
		accessible
			.name()
			.map_ok(|s| if s.is_empty() { None } else { Some(s) }),
		accessible
			.description()
			.map_ok(|s| if s.is_empty() { None } else { Some(s) }),
		accessible
			.help_text()
			.map_ok(|s| if s.is_empty() { None } else { Some(s) }),
		accessible
			.to_text()
			.and_then(|text_proxy| {
				text_proxy.get_all_text().map_ok(|s| {
					if s.is_empty() {
						None
					} else {
						Some(s)
					}
				})
			})
			.unwrap_or_else(|_| None)
			.map(Ok),
	)
		.try_join();
	let (
		(app, parent, index, children_num, interfaces, role, states, children),
		(name, description, help_text, text),
	) = (props, maps).try_join().await?;

	let ci = CacheItem {
		object: accessible.into(),
		app: app.into(),
		parent: parent.into(),
		index: index.try_into().ok(),
		children_num: children_num.try_into().ok(),
		interfaces,
		role,
		states,
		children: children.into_iter().map(Into::into).collect(),
		name,
		description,
		help_text,
		text,
	};
	Ok(ci)
}

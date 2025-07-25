use std::{
	collections::{btree_map::Entry, BTreeMap},
	fmt::Debug,
	marker::PhantomData,
	task::{Context, Poll},
};

use atspi::{
	events::{DBusInterface, DBusMember},
	Event, EventTypeProperties,
};
use futures_util::{
	future::{err, Either, ErrInto, Ready},
	TryFutureExt,
};
use odilia_common::{
	command::{
		CommandType, CommandTypeDynamic, OdiliaCommand as Command,
		OdiliaCommandDiscriminants as CommandDiscriminants,
	},
	errors::OdiliaError,
	events::{EventType, EventTypeDynamic, ScreenReaderEvent, ScreenReaderEventDiscriminants},
};
use tower::Service;

pub trait Chooser<K> {
	fn identifier(&self) -> K;
}
pub trait ChooserStatic<K> {
	fn identifier() -> K;
}

#[allow(clippy::module_name_repetitions)]
pub struct ChoiceService<K, S, Req>
where
	S: Service<Req>,
	Req: Chooser<K>,
{
	services: BTreeMap<K, S>,
	_marker: PhantomData<Req>,
}

impl<K, S, Req> Clone for ChoiceService<K, S, Req>
where
	K: Clone,
	S: Clone + Service<Req>,
	Req: Chooser<K>,
{
	fn clone(&self) -> Self {
		ChoiceService { services: self.services.clone(), _marker: PhantomData }
	}
}

impl<K, S, Req> ChoiceService<K, S, Req>
where
	S: Service<Req>,
	Req: Chooser<K>,
{
	pub fn new() -> Self {
		ChoiceService { services: BTreeMap::new(), _marker: PhantomData }
	}
	pub fn entry(&mut self, k: K) -> Entry<'_, K, S>
	where
		K: Ord,
	{
		self.services.entry(k)
	}
}

impl<K, S, Req> Service<Req> for ChoiceService<K, S, Req>
where
	S: Service<Req> + Clone,
	Req: Chooser<K>,
	K: Ord + Debug,
	OdiliaError: From<S::Error>,
{
	type Response = S::Response;
	type Error = OdiliaError;
	type Future =
		Either<Ready<Result<Self::Response, Self::Error>>, ErrInto<S::Future, OdiliaError>>;
	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		for (_k, svc) in &mut self.services.iter_mut() {
			let _ = svc.poll_ready(cx)?;
		}
		Poll::Ready(Ok(()))
	}
	fn call(&mut self, req: Req) -> Self::Future {
		let k = req.identifier();

		let mut svc = if let Some(orig_svc) = self.services.get_mut(&k) {
			let clone = orig_svc.clone();
			std::mem::replace(orig_svc, clone)
		} else {
			return Either::Left(err(OdiliaError::ServiceNotFound(
                format!("A service with key {k:?} could not be found in a list with keys of {:?}", self.services.keys())
            )));
		};
		Either::Right(svc.call(req).err_into())
	}
}

impl<E> ChooserStatic<(&'static str, &'static str)> for E
where
	E: DBusInterface + DBusMember,
{
	fn identifier() -> (&'static str, &'static str) {
		(E::DBUS_INTERFACE, E::DBUS_MEMBER)
	}
}
impl<C> ChooserStatic<CommandDiscriminants> for C
where
	C: CommandType,
{
	fn identifier() -> CommandDiscriminants {
		C::CTYPE
	}
}
impl<E> ChooserStatic<ScreenReaderEventDiscriminants> for E
where
	E: EventType,
{
	fn identifier() -> ScreenReaderEventDiscriminants {
		E::ETYPE
	}
}

impl Chooser<(&'static str, &'static str)> for Event {
	fn identifier(&self) -> (&'static str, &'static str) {
		(self.interface(), self.member())
	}
}
impl Chooser<CommandDiscriminants> for Command {
	fn identifier(&self) -> CommandDiscriminants {
		self.ctype()
	}
}
impl Chooser<ScreenReaderEventDiscriminants> for ScreenReaderEvent {
	fn identifier(&self) -> ScreenReaderEventDiscriminants {
		self.etype()
	}
}

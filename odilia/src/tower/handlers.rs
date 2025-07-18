#![allow(dead_code)]

use std::{fmt::Debug, sync::Arc};

use async_channel::Receiver;
use atspi::{AtspiError, Event, EventProperties, EventTypeProperties};
use odilia_common::{
	command::{
		CommandType, OdiliaCommand as Command,
		OdiliaCommandDiscriminants as CommandDiscriminants, TryIntoCommands,
	},
	errors::OdiliaError,
	events::{EventType, ScreenReaderEvent, ScreenReaderEventDiscriminants},
};
use tower::{util::BoxCloneService, Service, ServiceExt};

use crate::{
	or_cancel,
	state::ScreenReaderState,
	tower::{
		async_try::AsyncTryFrom,
		choice::{ChoiceService, ChooserStatic},
		from_state::TryFromState,
		service_set::ServiceSet,
		Handler, ServiceExt as OdiliaServiceExt,
	},
};

type Response = Vec<Command>;
type Request = Event;
type Error = OdiliaError;
type OdiliaResult<T> = Result<T, OdiliaError>;
type ResultList = Vec<OdiliaResult<()>>;

type AtspiHandler = BoxCloneService<Event, (), Error>;
type CommandHandler = BoxCloneService<Command, (), Error>;
type InputHandler = BoxCloneService<ScreenReaderEvent, (), Error>;

#[derive(Clone)]
pub struct Handlers {
	state: Arc<ScreenReaderState>,
	atspi: ChoiceService<(&'static str, &'static str), ServiceSet<AtspiHandler>, Event>,
	command: ChoiceService<CommandDiscriminants, ServiceSet<CommandHandler>, Command>,
	input: ChoiceService<
		ScreenReaderEventDiscriminants,
		ServiceSet<InputHandler>,
		ScreenReaderEvent,
	>,
}

impl Handlers {
	pub fn new(state: Arc<ScreenReaderState>) -> Self {
		Handlers {
			state,
			atspi: ChoiceService::new(),
			command: ChoiceService::new(),
			input: ChoiceService::new(),
		}
	}
	pub async fn command_handler(mut self, commands: Receiver<Command>) {
		loop {
			let maybe_cmd = commands.recv().await;
			let Ok(cmd) = maybe_cmd else {
				tracing::error!("Error cmd: {maybe_cmd:?}");
				continue;
			};
			// NOTE: Why not use join_all(...) ?
			// Because this drives the futures concurrently, and we want ordered handlers.
			// Otherwise, we cannot guarentee that the caching functions get run first.
			// we could move caching to a separate, ordered system, then parallelize the other functions,
			// if we determine this is a performance problem.
			if let Err(e) = self.command.call(cmd).await {
				tracing::error!("{e:?}");
			}
		}
	}
	#[tracing::instrument(skip_all)]
	pub async fn atspi_handler(
		mut self,
		events: Receiver<Result<Event, AtspiError>>,
		token: crate::CancellationToken,
	) {
		loop {
			let maybe = or_cancel(events.recv(), &token).await;
			let Ok(maybe_ev) = maybe else {
				return;
			};
			let Ok(Ok(ev)) = maybe_ev else {
				tracing::error!("Error in processing {maybe_ev:?}");
				continue;
			};
			if let Err(e) = self.atspi.call(ev).await {
				tracing::error!("{e:?}");
			}
		}
	}
	#[tracing::instrument(skip_all)]
	pub async fn input_handler(
		mut self,
		mut events: Receiver<ScreenReaderEvent>,
		token: crate::CancellationToken,
	) {
		std::pin::pin!(&mut events);
		loop {
			let maybe = or_cancel(events.recv(), &token).await;
			let Ok(maybe_ev) = maybe else {
				return;
			};
			tracing::info!("INPUT: {:?}", maybe_ev);
			let Ok(ev) = maybe_ev else {
				tracing::error!("Error in processing {maybe_ev:?}");
				continue;
			};
			if let Err(e) = self.input.call(ev).await {
				tracing::error!("{e:?}");
			}
		}
	}
	pub fn command_listener<H, T, C, R>(mut self, handler: H) -> Self
	where
		H: Handler<T, Response = R> + Send + Clone + 'static,
		<H as Handler<T>>::Future: Send,
		C: CommandType + ChooserStatic<CommandDiscriminants> + Send + 'static,
		Command: TryInto<C>,
		OdiliaError: From<<Command as TryInto<C>>::Error>
			+ From<<T as TryFromState<Arc<ScreenReaderState>, C>>::Error>
			+ From<<T as AsyncTryFrom<(Arc<ScreenReaderState>, C)>>::Error>,
		R: Into<Result<(), Error>> + Send + 'static,
		T: TryFromState<Arc<ScreenReaderState>, C> + Send + 'static,
		<T as TryFromState<Arc<ScreenReaderState>, C>>::Future: Send,
		<T as TryFromState<Arc<ScreenReaderState>, C>>::Error: Send,
		T: AsyncTryFrom<(Arc<ScreenReaderState>, C)>,
		<T as AsyncTryFrom<(Arc<ScreenReaderState>, C)>>::Future: std::marker::Send,
	{
		let bs = handler
			.into_service()
			.map_response_into::<R, (), OdiliaError>()
			.request_async_try_from()
			.with_state(Arc::clone(&self.state))
			.request_try_from()
			.boxed_clone();
		self.command.entry(C::identifier()).or_default().push(bs);
		Self {
			state: self.state,
			atspi: self.atspi,
			command: self.command,
			input: self.input,
		}
	}
	pub fn input_listener<H, T, R, E>(mut self, handler: H) -> Self
	where
		H: Handler<T, Response = R> + Send + Clone + 'static,
		<H as Handler<T>>::Future: Send,
		E: EventType + ChooserStatic<ScreenReaderEventDiscriminants> + Send + 'static,
		ScreenReaderEvent: TryInto<E>,
		OdiliaError: From<<ScreenReaderEvent as TryInto<E>>::Error>
			+ From<<T as TryFromState<Arc<ScreenReaderState>, E>>::Error>,
		R: TryIntoCommands + Send + 'static,
		T: TryFromState<Arc<ScreenReaderState>, E> + Send + 'static,
		<T as TryFromState<Arc<ScreenReaderState>, E>>::Future: Send,
		<T as TryFromState<Arc<ScreenReaderState>, E>>::Error: Send,
	{
		let bs = handler
			.into_service()
			.map_response_try_into_command()
			.request_async_try_from()
			.with_state(Arc::clone(&self.state))
			.request_try_from()
			.iter_into(self.command.clone())
			.map_result(|res: OdiliaResult<Vec<OdiliaResult<ResultList>>>| {
				res?.into_iter() // Remove outer result
					.flatten() // Flatten out first vec
					.flatten() // Flatten out ResultList
					.collect::<Result<(), OdiliaError>>()
			})
			.boxed_clone();
		self.input.entry(E::identifier()).or_default().push(bs);
		Self {
			state: self.state,
			atspi: self.atspi,
			command: self.command,
			input: self.input,
		}
	}
	pub fn atspi_listener<H, T, R, E>(mut self, handler: H) -> Self
	where
		H: Handler<T, Response = R> + Send + Clone + 'static,
		<H as Handler<T>>::Future: Send,
		E: EventTypeProperties
			+ Debug
			+ EventProperties
			+ TryFrom<Event>
			+ EventProperties
			+ ChooserStatic<(&'static str, &'static str)>
			+ Clone
			+ Send
			+ 'static,
		OdiliaError: From<<Event as TryInto<E>>::Error>
			+ From<<T as TryFromState<Arc<ScreenReaderState>, E>>::Error>
			+ From<<T as AsyncTryFrom<(Arc<ScreenReaderState>, E)>>::Error>,
		R: TryIntoCommands + Send + 'static,
		T: TryFromState<Arc<ScreenReaderState>, E> + Send + 'static,
		<T as TryFromState<Arc<ScreenReaderState>, E>>::Error: Send + 'static,
		<T as TryFromState<Arc<ScreenReaderState>, E>>::Future: Send,
		T: AsyncTryFrom<(Arc<ScreenReaderState>, E)>,
		<T as AsyncTryFrom<(Arc<ScreenReaderState>, E)>>::Future: std::marker::Send,
	{
		let bs = handler
			.into_service()
			.map_response_try_into_command()
			.request_async_try_from()
			.with_state(Arc::clone(&self.state))
			.request_try_from()
			.iter_into(self.command.clone())
			// TODO: do this without a bunch of allocation.
			.map_result(|res: OdiliaResult<Vec<OdiliaResult<ResultList>>>| {
				res?.into_iter() // Remove outer result
					.flatten() // Flatten out first vec
					.flatten() // Flatten out ResultList
					.collect::<Result<(), OdiliaError>>()
			})
			.boxed_clone();
		self.atspi.entry(E::identifier()).or_default().push(bs);
		Self {
			state: self.state,
			atspi: self.atspi,
			command: self.command,
			input: self.input,
		}
	}
}

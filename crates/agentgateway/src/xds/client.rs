use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use std::time::Duration;
use std::{fmt, mem};

use itertools::Itertools;
use pbjson_types::value::Kind;
use pbjson_types::{Struct, Value};
use prost::{DecodeError, EncodeError};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::{Instrument, debug, error, info, info_span, warn};

use super::envoy::service::discovery::v3::Resource as ProtoResource;
use super::envoy::service::discovery::v3::aggregated_discovery_service_client::AggregatedDiscoveryServiceClient;
use super::envoy::service::discovery::v3::*;
use crate::xds::metrics::{ConnectionTerminationReason, Metrics};
use agent_core::metrics::{IncrementRecorder, Recorder};
use agent_core::strng;
use agent_core::strng::Strng;

use super::Error;

const INSTANCE_IP: &str = "INSTANCE_IP";
const INSTANCE_IPS: &str = "INSTANCE_IPS";
const DEFAULT_IP: &str = "1.1.1.1";
const POD_NAME: &str = "POD_NAME";
const POD_NAMESPACE: &str = "POD_NAMESPACE";
const NODE_NAME: &str = "NODE_NAME";
const ROLE: &str = "role";
const GW_NAME: &str = "GW_NAME";
const NAME: &str = "NAME";
const NAMESPACE: &str = "NAMESPACE";
const EMPTY_STR: &str = "";

#[derive(Eq, Hash, PartialEq, Debug, Clone)]
pub struct ResourceKey {
	pub name: Strng,
	pub type_url: Strng,
}

impl Display for ResourceKey {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		write!(f, "{}/{}", self.type_url, self.name)
	}
}

#[derive(Debug)]
pub struct RejectedConfig {
	name: Strng,
	reason: anyhow::Error,
}

impl RejectedConfig {
	pub fn new(name: Strng, reason: anyhow::Error) -> Self {
		Self { name, reason }
	}
}

impl Display for RejectedConfig {
	fn fmt(&self, f: &mut Formatter) -> fmt::Result {
		write!(f, "{}: {}", self.name, self.reason)
	}
}

/// handle_single_resource is a helper to process a set of updates with an async closure.
/// It handles aggregating errors as NACKS.
pub async fn handle_single_resource<
	T: prost::Message + Send,
	Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
	F: FnMut(XdsUpdate<T>) -> Fut + Send + Sync,
>(
	updates: Vec<XdsUpdate<T>>,
	mut handle_one: F,
) -> Result<(), Vec<RejectedConfig>> {
	let mut rejects = Vec::new();
	for res in updates {
		let name = res.name();
		if let Err(e) = handle_one(res).await {
			rejects.push(RejectedConfig::new(name, e));
		}
	}

	if rejects.is_empty() {
		Ok(())
	} else {
		Err(rejects)
	}
}

// Handler is responsible for handling a discovery response.
// Handlers can mutate state and return a list of rejected configurations (if there are any).
#[async_trait::async_trait]
pub trait Handler<T: prost::Message + Send>: Send + Sync + 'static {
	fn no_on_demand(&self) -> bool {
		false
	}

	async fn handle(&self, res: Vec<XdsUpdate<T>>) -> Result<(), Vec<RejectedConfig>>;
}

// ResponseHandler is responsible for handling a discovery response.
// Handlers can mutate state and return a list of rejected configurations (if there are any).
// This is an internal only trait; public usage uses the Handler type which is typed.
#[async_trait::async_trait]
trait RawHandler: Send + Sync + 'static {
	async fn handle(
		&self,
		state: &mut State,
		res: DeltaDiscoveryResponse,
	) -> Result<(), Vec<RejectedConfig>>;
}

// HandlerWrapper is responsible for implementing RawHandler the provided handler.
struct HandlerWrapper<T: prost::Message> {
	h: Box<dyn Handler<T>>,
}

#[async_trait::async_trait]
impl<T: 'static + prost::Message + Default> RawHandler for HandlerWrapper<T> {
	async fn handle(
		&self,
		state: &mut State,
		res: DeltaDiscoveryResponse,
	) -> Result<(), Vec<RejectedConfig>> {
		let type_url = strng::new(res.type_url);

		let removes = res.removed_resources.clone();

		// TODO: Less list mangling

		// Keep track of any failures but keep going
		let (updates, decode_failures): (Vec<_>, Vec<_>) = res
			.resources
			.iter()
			.map(|raw| {
				decode_proto::<T>(raw).map_err(|err| RejectedConfig {
					name: raw.name.as_str().into(),
					reason: err.into(),
				})
			})
			.map_ok(XdsUpdate::Update)
			.partition_result();

		// Collect removes explicitly to avoid borrowing `removes` in the chained iterator
		let remove_updates: Vec<_> = removes
			.iter()
			.cloned()
			.map(|s| XdsUpdate::Remove(s.into()))
			.collect();

		let final_updates = updates.into_iter().chain(remove_updates).collect();

		// First, call handlers that update the proxy state.
		// other wise on-demand notifications might observe a cache without their resource
		let result = self.h.handle(final_updates).await;

		// after we update the proxy cache, we can update our xds cache. it's important that we do this after
		// as we make on demand notifications here, so the proxy cache must be updated first.
		for name in removes {
			let k = ResourceKey {
				name: name.into(),
				type_url: type_url.clone(),
			};
			debug!("received delete resource {k}");
			if let Some(rm) = state.known_resources.get_mut(&k.type_url) {
				rm.remove(&k.name);
			}
			state.notify_on_demand(&k);
		}

		for r in res.resources {
			let key = ResourceKey {
				name: r.name.into(),
				type_url: type_url.clone(),
			};
			state.notify_on_demand(&key);
			state.add_resource(key.type_url, key.name);
		}

		// Either can fail. Merge the results
		match (result, decode_failures.is_empty()) {
			(Ok(()), true) => Ok(()),
			(Ok(_), false) => Err(decode_failures),
			(r @ Err(_), true) => r,
			(Err(mut rejects), false) => {
				rejects.extend(decode_failures);
				Err(rejects)
			},
		}
	}
}

pub struct Config {
	address: String,
	metadata: HashMap<String, String>,
	handlers: HashMap<Strng, Box<dyn RawHandler>>,
	initial_requests: Vec<DeltaDiscoveryRequest>,
	on_demand: bool,
}

pub struct State {
	/// Stores all known workload resources. Map from type_url to name
	known_resources: HashMap<Strng, HashSet<Strng>>,

	/// pending stores a list of all resources that are pending and XDS push
	pending: HashMap<ResourceKey, oneshot::Sender<()>>,

	demand: mpsc::Receiver<(oneshot::Sender<()>, ResourceKey)>,
	demand_tx: mpsc::Sender<(oneshot::Sender<()>, ResourceKey)>,
}

impl State {
	fn notify_on_demand(&mut self, key: &ResourceKey) {
		if let Some(send) = self.pending.remove(key) {
			debug!("on demand notify {}", key.name);
			if send.send(()).is_err() {
				warn!("on demand dropped event for {}", key.name)
			}
		}
	}
	fn add_resource(&mut self, type_url: Strng, name: Strng) {
		self
			.known_resources
			.entry(type_url)
			.or_default()
			.insert(name.clone());
	}
}

impl Config {
	pub fn new(config: Arc<super::Config>) -> Config {
		Config {
			address: config.xds_address.clone(),
			handlers: HashMap::new(),
			initial_requests: Vec::new(),
			on_demand: false,
			metadata: config.metadata.clone(),
		}
	}

	pub fn with_watched_handler<F>(self, type_url: Strng, f: impl Handler<F>) -> Config
	where
		F: 'static + prost::Message + Default,
	{
		let no_on_demand = f.no_on_demand();
		self
			.with_handler(type_url.clone(), f)
			.watch(type_url, no_on_demand)
	}

	fn with_handler<F>(mut self, type_url: Strng, f: impl Handler<F>) -> Config
	where
		F: 'static + prost::Message + Default,
	{
		let h = HandlerWrapper { h: Box::new(f) };
		self.handlers.insert(type_url, Box::new(h));
		self
	}

	fn watch(mut self, type_url: Strng, no_on_demand: bool) -> Config {
		self
			.initial_requests
			.push(self.construct_initial_request(type_url, no_on_demand));
		self
	}

	fn build_struct<T: IntoIterator<Item = (S, S)>, S: ToString>(a: T) -> Struct {
		let fields = HashMap::from_iter(a.into_iter().map(|(k, v)| {
			(
				k.to_string(),
				Value {
					kind: Some(Kind::StringValue(v.to_string())),
				},
			)
		}));
		Struct { fields }
	}

	fn node(&self) -> Node {
		let ip = std::env::var(INSTANCE_IP);
		let ip = ip.as_deref().unwrap_or(DEFAULT_IP);
		let gw_name = std::env::var(GW_NAME).unwrap_or_default();
		let pod_name = std::env::var(POD_NAME);
		let pod_name = pod_name.as_deref().unwrap_or(EMPTY_STR);
		let ns = std::env::var(POD_NAMESPACE);
		let ns = ns.as_deref().unwrap_or(EMPTY_STR);
		let node_name = std::env::var(NODE_NAME);
		let node_name = node_name.as_deref().unwrap_or(EMPTY_STR);

		let role = format!(
			"mcp-kgateway-kube-gateway-api~{ns}~{name}",
			ns = ns,
			name = gw_name
		);

		let mut metadata = Self::build_struct([
			(NAME, pod_name),
			(NAMESPACE, ns),
			(INSTANCE_IPS, ip),
			(NODE_NAME, node_name),
			(ROLE, &role),
		]);
		metadata
			.fields
			.extend(Self::build_struct(self.metadata.clone()).fields);

		Node {
			id: format!("mcpgw~{ip}~{pod_name}.{ns}~{ns}.svc.cluster.local"),
			metadata: Some(metadata),
			..Default::default()
		}
	}
	fn construct_initial_request(
		&self,
		request_type: Strng,
		no_on_demand: bool,
	) -> DeltaDiscoveryRequest {
		let node = self.node();

		let (sub, unsub) = if (!no_on_demand) && self.on_demand {
			// XDS doesn't have a way to subscribe to zero resources. We workaround this by subscribing and unsubscribing
			// in one event, effectively giving us "subscribe to nothing".
			(vec!["*".to_string()], vec!["*".to_string()])
		} else {
			(vec![], vec![])
		};
		DeltaDiscoveryRequest {
			type_url: request_type.to_string(),
			node: Some(node.clone()),
			resource_names_subscribe: sub,
			resource_names_unsubscribe: unsub,
			..Default::default()
		}
	}

	pub fn build(self, metrics: Metrics, block_ready: tokio::sync::watch::Sender<()>) -> AdsClient {
		AdsClient::new(self, metrics, block_ready)
	}
}

/// AdsClient provides a (mostly) generic DeltaAggregatedResources XDS client.
///
/// The client works by accepting arbitrary handlers for types, configured by user.
/// These handlers can do whatever they want with incoming responses, but are responsible for maintaining their own state.
/// For example, if a usage wants to keep track of all Foo resources received, it needs to handle the add/removes in the configured handler.
///
/// The client also supports on-demand lookup of resources; see demander() for more information.
///
/// Currently, this is not quite a fully general purpose XDS client, as there is no dependant resource support.
/// This could be added if needed, though.
pub struct AdsClient {
	config: Config,

	state: State,

	pub(crate) metrics: Metrics,
	block_ready: Option<tokio::sync::watch::Sender<()>>,

	connection_id: u32,
	types_to_expect: HashSet<String>,
}

/// Demanded allows awaiting for an on-demand XDS resource
pub struct Demanded {
	b: oneshot::Receiver<()>,
}

impl Demanded {
	/// recv awaits for the requested resource
	/// Note: the actual resource is not directly returned. Instead, callers are notified that the event
	/// has been handled through the configured resource handler.
	pub async fn recv(self) {
		let _ = self.b.await;
	}
}

/// Demander allows requesting XDS resources on-demand
#[derive(Debug, Clone)]
pub struct Demander {
	demand: mpsc::Sender<(oneshot::Sender<()>, ResourceKey)>,
}

#[derive(Debug)]
enum XdsSignal {
	Ack,
	Nack,
}

impl Display for XdsSignal {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			XdsSignal::Ack => "ACK",
			XdsSignal::Nack => "NACK",
		})
	}
}

impl Demander {
	/// Demand requests a given workload by name
	pub async fn demand(&self, type_url: Strng, name: Strng) -> Demanded {
		let (tx, rx) = oneshot::channel::<()>();
		self
			.demand
			.send((tx, ResourceKey { name, type_url }))
			.await
			// TODO: is this guaranteed? How can we handle the failure
			.expect("demand channel should not close");
		Demanded { b: rx }
	}
}

const INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const MAX_BACKOFF: Duration = Duration::from_secs(15);

impl AdsClient {
	fn is_initial_request_on_demand(r: &DeltaDiscoveryRequest) -> bool {
		!r.resource_names_subscribe.is_empty()
	}

	fn new(config: Config, metrics: Metrics, block_ready: tokio::sync::watch::Sender<()>) -> Self {
		let (tx, rx) = mpsc::channel(100);
		let state = State {
			known_resources: Default::default(),
			pending: Default::default(),
			demand: rx,
			demand_tx: tx,
		};
		let types_to_expect: HashSet<String> = config
			.initial_requests
			.iter()
			.filter(|e| !Self::is_initial_request_on_demand(e)) // is_empty implies not ondemand
			.map(|e| e.type_url.clone())
			.collect();
		AdsClient {
			config,
			state,
			metrics,
			block_ready: Some(block_ready),
			connection_id: 0,
			types_to_expect,
		}
	}

	/// demander returns a Demander instance which can be used to request resources on-demand
	pub fn demander(&self) -> Option<Demander> {
		if self.config.on_demand {
			Some(Demander {
				demand: self.state.demand_tx.clone(),
			})
		} else {
			None
		}
	}

	async fn run_loop(&mut self, backoff: Duration) -> Duration {
		match self.run_internal().await {
			Err(e @ Error::Connection(_, _)) => {
				// For connection errors, we add backoff
				let backoff = std::cmp::min(MAX_BACKOFF, backoff * 2);
				warn!(
					"XDS client connection error: {}, retrying in {:?}",
					e, backoff
				);
				self
					.metrics
					.increment(&ConnectionTerminationReason::ConnectionError);
				tokio::time::sleep(backoff).await;
				backoff
			},
			Err(ref e @ Error::GrpcStatus(ref status)) => {
				let err_detail = e.to_string();
				let backoff = if status.code() == tonic::Code::Unknown
					|| status.code() == tonic::Code::Cancelled
					|| status.code() == tonic::Code::DeadlineExceeded
					|| (status.code() == tonic::Code::Unavailable
						&& status.message().contains("transport is closing"))
					|| (status.code() == tonic::Code::Unavailable
						&& status.message().contains("received prior goaway"))
				{
					debug!(
						"XDS client terminated: {}, retrying in {:?}",
						err_detail, backoff
					);
					self
						.metrics
						.increment(&ConnectionTerminationReason::Reconnect);
					INITIAL_BACKOFF
				} else {
					warn!(
						"XDS client error: {}, retrying in {:?}",
						err_detail, backoff
					);
					self.metrics.increment(&ConnectionTerminationReason::Error);
					// For gRPC errors, we add backoff
					std::cmp::min(MAX_BACKOFF, backoff * 2)
				};
				tokio::time::sleep(backoff).await;
				backoff
			},
			Err(e) => {
				// For other errors, we connect immediately
				// TODO: we may need more nuance here; if we fail due to invalid initial request we may overload
				// But we want to reconnect from MaxConnectionAge immediately.
				warn!("XDS client error: {:?}, retrying", e);
				self.metrics.increment(&ConnectionTerminationReason::Error);
				// Reset backoff
				INITIAL_BACKOFF
			},
			Ok(_) => {
				self
					.metrics
					.increment(&ConnectionTerminationReason::Complete);
				warn!("XDS client complete");
				// Reset backoff
				INITIAL_BACKOFF
			},
		}
	}

	pub async fn run(mut self) -> Result<(), Error> {
		let mut backoff = INITIAL_BACKOFF;
		loop {
			self.connection_id += 1;
			let id = self.connection_id;
			backoff = self
				.run_loop(backoff)
				.instrument(info_span!("xds", id))
				.await;
		}
	}

	async fn run_internal(&mut self) -> Result<(), Error> {
		let (discovery_req_tx, mut discovery_req_rx) = mpsc::channel::<DeltaDiscoveryRequest>(100);
		// For each type in initial_watches we will send a request on connection to subscribe
		let initial_requests: Vec<DeltaDiscoveryRequest> = self
			.config
			.initial_requests
			.iter()
			.map(|e| {
				let mut req = e.clone();
				req.initial_resource_versions = self
					.state
					.known_resources
					.get(&strng::new(&req.type_url))
					.map(|hs| {
						hs.iter()
							.map(|n| (n.to_string(), "".to_string())) // Proto expects Name -> Version. We don't care about version
							.collect()
					})
					.unwrap_or_default();
				req
			})
			.collect();

		let outbound = async_stream::stream! {
				for initial in initial_requests {
						debug!(resources=initial.initial_resource_versions.len(), type_url=initial.type_url, "sending initial request");
						yield initial;
				}
				while let Some(message) = discovery_req_rx.recv().await {
						debug!(type_url=message.type_url, "sending request");
						yield message
				}
				warn!("outbound stream complete");
		};

		let addr = self.config.address.clone();
		let req = tonic::Request::new(outbound);
		let ads_connection = AggregatedDiscoveryServiceClient::connect(self.config.address.clone())
			.await?
			.max_decoding_message_size(200 * 1024 * 1024)
			.delta_aggregated_resources(req)
			.await;

		let mut response_stream = ads_connection
			.map_err(|src| Error::Connection(addr, src))?
			.into_inner();
		debug!("connected established");

		info!("Stream established");
		loop {
			tokio::select! {
					_demand_event = self.state.demand.recv() => {
							self.handle_demand_event(_demand_event, &discovery_req_tx).await?;
					}
					msg = response_stream.message() => {
							let Some(msg) = msg? else {
									// If we got a None message, the stream ended without error.
									// This could be an explicit OK response, or if the stream is reset without a gRPC status.
									return Ok(());
							};
							let mut received_type = None;
							if !self.types_to_expect.is_empty() {
									received_type = Some(msg.type_url.clone())
							}
							if let XdsSignal::Ack = self.handle_stream_event(msg, &discovery_req_tx).await? {
									if let Some(received_type) = received_type {
											self.types_to_expect.remove(&received_type);
											if self.types_to_expect.is_empty() {
													mem::drop(mem::take(&mut self.block_ready));
											}
									}
							};
					}
			}
		}
	}

	async fn handle_stream_event(
		&mut self,
		response: DeltaDiscoveryResponse,
		send: &mpsc::Sender<DeltaDiscoveryRequest>,
	) -> Result<XdsSignal, Error> {
		let type_url = response.type_url.clone();
		let nonce = response.nonce.clone();
		self.metrics.record(&response, ());
		info!(
			type_url = type_url, // this is a borrow, it's OK
			size = response.resources.len(),
			removes = response.removed_resources.len(),
			"received response"
		);
		let handler_response: Result<(), Vec<RejectedConfig>> =
			match self.config.handlers.get(&strng::new(&type_url)) {
				Some(h) => h.handle(&mut self.state, response).await,
				None => {
					error!(%type_url, "unknown type");
					// TODO: this will just send another discovery request, to server. We should
					// either send one with an error or not send one at all.
					Ok(())
				},
			};

		let (response_type, error) = match handler_response {
			Err(rejects) => {
				let error = rejects
					.into_iter()
					.map(|reject| reject.to_string())
					.collect::<Vec<String>>()
					.join("; ");
				(XdsSignal::Nack, Some(error))
			},
			_ => (XdsSignal::Ack, None),
		};

		match response_type {
			XdsSignal::Nack => error!(
					type_url=type_url,
					nonce,
					"type"=?response_type,
					error=error,
					"sending response",
			),
			_ => debug!(
					type_url=type_url,
					nonce,
					"type"=?response_type,
					"sending response",
			),
		};

		send
			.send(DeltaDiscoveryRequest {
				type_url,              // this is owned, OK to move
				response_nonce: nonce, // this is owned, OK to move
				error_detail: error.map(|msg| Status {
					message: msg,
					..Default::default()
				}),
				..Default::default()
			})
			.await
			.map_err(|e| Error::RequestFailure(Box::new(e)))
			.map(|_| response_type)
	}

	async fn handle_demand_event(
		&mut self,
		demand_event: Option<(oneshot::Sender<()>, ResourceKey)>,
		send: &mpsc::Sender<DeltaDiscoveryRequest>,
	) -> Result<(), Error> {
		let Some((tx, demand_event)) = demand_event else {
			return Ok(());
		};
		info!("received on demand request {demand_event}");
		let ResourceKey { type_url, name } = demand_event.clone();
		self.state.pending.insert(demand_event, tx);
		self.state.add_resource(type_url.clone(), name.clone());
		send
			.send(DeltaDiscoveryRequest {
				type_url: type_url.to_string(),
				resource_names_subscribe: vec![name.to_string()],
				..Default::default()
			})
			.await
			.map_err(|e| Error::RequestFailure(Box::new(e)))?;
		Ok(())
	}
}

#[derive(Clone, Debug)]
pub struct XdsResource<T: prost::Message> {
	pub name: Strng,
	pub resource: T,
}

#[derive(Debug)]
pub enum XdsUpdate<T: prost::Message> {
	Update(XdsResource<T>),
	Remove(Strng),
}

impl<T: prost::Message> XdsUpdate<T> {
	pub fn name(&self) -> Strng {
		match self {
			XdsUpdate::Update(r) => r.name.clone(),
			XdsUpdate::Remove(n) => n.clone(),
		}
	}
}

fn decode_proto<T: prost::Message + Default>(
	resource: &ProtoResource,
) -> Result<XdsResource<T>, AdsError> {
	let name = resource.name.as_str().into();
	resource
		.resource
		.as_ref()
		.ok_or(AdsError::MissingResource())
		.and_then(|res| <T>::decode(&res.value[..]).map_err(AdsError::Decode))
		.map(|r| XdsResource { name, resource: r })
}

#[derive(Clone, Debug, Error)]
pub enum AdsError {
	#[error("unknown resource type: {0}")]
	UnknownResourceType(String),
	#[error("decode: {0}")]
	Decode(#[from] DecodeError),
	#[error("XDS payload without resource")]
	MissingResource(),
	#[error("encode: {0}")]
	Encode(#[from] EncodeError),
}

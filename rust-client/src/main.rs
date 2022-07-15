use clap::Parser;
use futures::future::FutureExt;
use futures::stream::StreamExt;
use libp2p::core::multiaddr::{Multiaddr, Protocol};
use libp2p::core::transport::OrTransport;
use libp2p::core::{upgrade, ConnectedPoint, ProtocolName, UpgradeInfo};
use libp2p::dcutr;
use libp2p::dcutr::behaviour::UpgradeError;
use libp2p::dns::DnsConfig;
use libp2p::identify::{Identify, IdentifyConfig, IdentifyEvent};
use libp2p::noise;
use libp2p::ping::{Ping, PingConfig, PingEvent};
use libp2p::relay::v2::client::{self, Client};
use libp2p::swarm::dial_opts::DialOpts;
use libp2p::swarm::{
    ConnectionHandlerUpgrErr, IntoConnectionHandler, NetworkBehaviour, Swarm, SwarmBuilder,
    SwarmEvent,
};
use libp2p::tcp::{TcpTransport, GenTcpConfig};
use libp2p::Transport;
use libp2p::{identity, PeerId};
use log::{info, warn};
use rustls::client::{
    ClientConfig, HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use std::convert::TryInto;
use std::env;
use std::net::Ipv4Addr;
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod grpc {
    tonic::include_proto!("_");
}

fn agent_version() -> String {
    format!("punchr/rust-client/{}", env!("CARGO_PKG_VERSION"))
}

#[derive(Parser, Debug)]
struct Opt {
    #[clap(long)]
    url: String,

    /// Fixed value to generate deterministic peer id.
    #[clap(long)]
    secret_key_seed: u8,

    /// Number of iterations to run.
    /// Will loop eternally if set to `None`.
    #[clap(long)]
    rounds: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = env_logger::try_init();

    // Api-key used to authenticate our client at the server.
    let api_key = match env::var("API_KEY") {
        Ok(k) => k,
        Err(env::VarError::NotPresent) => {
            warn!("No value for env variable \"API_KEY\" found. If the server enforces authorization it will reject our requests.");
            String::new()
        }
        Err(e) => return Err(e.into()),
    };

    let opt = Opt::parse();

    // Custom tls client config that accepts all certificates.
    let tls = ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(Arc::new(DummyVerifyAll))
        .with_no_client_auth();
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http2()
        .build();
    let conn = tonic::transport::Endpoint::new(opt.url.clone())?
        .connect_with_connector(connector)
        .await?;
    let mut client = grpc::punchr_service_client::PunchrServiceClient::new(conn);

    info!("Connected to server {}.", opt.url);

    let local_key = generate_ed25519(opt.secret_key_seed);
    let local_peer_id = PeerId::from(local_key.public());
    info!("Local peer id: {:?}", local_peer_id);

    let mut protocols = None;

    let mut iterations = opt.rounds.map(|r| (0, r));

    #[allow(clippy::blocks_in_if_conditions)]
    while iterations
        .as_mut()
        .map(|(i, r)| {
            *i += 1;
            i <= r
        })
        .unwrap_or(true)
    {
        let mut swarm = init_swarm(local_key.clone()).await?;
        if protocols.is_none() {
            let supported_protocols = swarm
                .behaviour_mut()
                .new_handler()
                .inbound_protocol()
                .protocol_info()
                .into_iter()
                .map(|info| String::from_utf8(info.protocol_name().to_vec()))
                .collect::<Result<Vec<String>, _>>()?;
            let _ = protocols.insert(supported_protocols);
        }

        let request = tonic::Request::new(grpc::RegisterRequest {
            client_id: local_peer_id.to_bytes(),
            agent_version: agent_version(),
            protocols: protocols.clone().unwrap(),
            api_key: api_key.clone(),
        });

        client.register(request).await?;

        let request = tonic::Request::new(grpc::GetAddrInfoRequest {
            host_id: local_peer_id.to_bytes(),
            all_host_ids: vec![local_peer_id.to_bytes()],
            api_key: api_key.clone(),
        });

        let response = client.get_addr_info(request).await?.into_inner();

        let remote_peer_id = PeerId::from_bytes(&response.remote_id)?;
        let remote_addrs = response
            .multi_addresses
            .clone()
            .into_iter()
            .map(Multiaddr::try_from)
            .collect::<Result<Vec<_>, libp2p::multiaddr::Error>>()?;

        let filter_remote_addrs: Vec<Multiaddr> = remote_addrs
            .into_iter()
            .filter(|a| !a.iter().any(|p| p == libp2p::multiaddr::Protocol::Quic))
            .collect();

        let state = HolePunchState::new(
            local_peer_id,
            swarm.listeners(),
            remote_peer_id,
            filter_remote_addrs.iter().map(|a| a.to_vec()).collect(),
            api_key.clone(),
        );

        if filter_remote_addrs.is_empty() {
            let request = state.cancel();
            client
                .track_hole_punch(tonic::Request::new(request))
                .await?;
            continue;
        }

        info!(
            "Attempting to punch through to {:?} via {:?}.",
            remote_peer_id, filter_remote_addrs
        );

        swarm.dial(
            DialOpts::peer_id(remote_peer_id)
                .addresses(filter_remote_addrs)
                .build(),
        )?;

        let request = state.drive_hole_punch(&mut swarm).await;

        client
            .track_hole_punch(tonic::Request::new(request))
            .await?;

        futures_timer::Delay::new(std::time::Duration::from_secs(1)).await;
    }

    Ok(())
}

async fn init_swarm(
    local_key: identity::Keypair,
) -> Result<Swarm<Behaviour>, Box<dyn std::error::Error>> {
    let local_peer_id = PeerId::from(local_key.public());

    let noise_keys = noise::Keypair::<noise::X25519Spec>::new()
        .into_authentic(&local_key)
        .expect("Signing libp2p-noise static DH keypair failed.");


    let (relay_transport, relay_behaviour) = Client::new_transport_and_behaviour(local_peer_id);

    let transport = OrTransport::new(
        relay_transport,
        DnsConfig::system(TcpTransport::new(GenTcpConfig::new().port_reuse(true))).await?,
    )
    .upgrade(upgrade::Version::V1)
    .authenticate(noise::NoiseConfig::xx(noise_keys).into_authenticated())
    .multiplex(libp2p::yamux::YamuxConfig::default())
    .boxed();

    let identify_config = IdentifyConfig::new("/ipfs/0.1.0".to_string(), local_key.public())
        .with_agent_version(agent_version());

    let behaviour = Behaviour {
        relay: relay_behaviour,
        ping: Ping::new(PingConfig::new()),
        identify: Identify::new(identify_config),
        dcutr: dcutr::behaviour::Behaviour::new(),
    };

    let mut swarm = SwarmBuilder::new(transport, behaviour, local_peer_id)
        .dial_concurrency_factor(10_u8.try_into()?)
        .build();

    swarm.listen_on(
        Multiaddr::empty()
            .with("0.0.0.0".parse::<Ipv4Addr>()?.into())
            .with(Protocol::Tcp(0)),
    )?;

    // Wait to listen on all interfaces.
    let mut delay = futures_timer::Delay::new(std::time::Duration::from_secs(1)).fuse();
    loop {
        futures::select! {
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!("Listening on {:?}", address);
                    }
                    event => panic!("{:?}", event),
                }
            }
            _ = delay => {
                // Likely listening on all interfaces now, thus continuing by breaking the loop.
                break;
            }
        }
    }

    Ok(swarm)
}

struct HolePunchState {
    request: grpc::TrackHolePunchRequest,
    active_holepunch_attempt: Option<HolePunchAttemptState>,
    remote_id: PeerId,
}

impl HolePunchState {
    fn new<'a>(
        client_id: PeerId,
        client_listen_addrs: impl Iterator<Item = &'a Multiaddr>,
        remote_id: PeerId,
        remote_multi_addresses: Vec<Vec<u8>>,
        api_key: String,
    ) -> Self {
        let request = grpc::TrackHolePunchRequest {
            client_id: client_id.into(),
            remote_id: remote_id.into(),
            remote_multi_addresses,
            open_multi_addresses: Vec::new(),
            has_direct_conns: false,
            connect_started_at: unix_time_now(),
            connect_ended_at: 0,
            hole_punch_attempts: Vec::new(),
            error: None,
            outcome: grpc::HolePunchOutcome::Unknown.into(),
            ended_at: 0,
            listen_multi_addresses: client_listen_addrs.map(|a| a.to_vec()).collect(),
            api_key,
        };
        HolePunchState {
            request,
            remote_id,
            active_holepunch_attempt: None,
        }
    }

    fn cancel(mut self) -> grpc::TrackHolePunchRequest {
        info!(
            "Skipping hole punch through to {:?} via {:?} because the Quic transport is not supported.",
            self.remote_id, self.request.remote_multi_addresses.iter().map(|a| Multiaddr::try_from(a.clone()).unwrap()).collect::<Vec<_>>()
        );
        let unix_time_now = unix_time_now();
        self.request.connect_ended_at = unix_time_now;
        self.request.ended_at = unix_time_now;
        self.request.error = Some("rust-lib2p doesn't support quic transport yet.".into());
        self.request.outcome = grpc::HolePunchOutcome::Cancelled.into();
        self.request
    }

    async fn drive_hole_punch(
        mut self,
        swarm: &mut libp2p::swarm::Swarm<Behaviour>,
    ) -> grpc::TrackHolePunchRequest {
        let (outcome, error) = loop {
            match swarm.select_next_some().await {
                SwarmEvent::NewListenAddr { address, .. } => info!("Listening on {:?}", address),
                SwarmEvent::Behaviour(Event::Relay(event)) => info!("{:?}", event),
                SwarmEvent::Behaviour(Event::Dcutr(event)) => {
                    info!("{:?}", event);
                    if let ControlFlow::Break((outcome, error)) = self.handle_dcutr_event(event) {
                        break (outcome, error);
                    }
                }
                SwarmEvent::Behaviour(Event::Identify(event)) => info!("{:?}", event),
                SwarmEvent::Behaviour(Event::Ping(_)) => {}
                SwarmEvent::ConnectionEstablished {
                    peer_id,
                    endpoint,
                    num_established,
                    ..
                } => {
                    info!("Established connection to {:?} via {:?}", peer_id, endpoint);
                    if peer_id == self.remote_id {
                        if let ControlFlow::Break((outcome, error)) =
                            self.handle_established_connection(endpoint, num_established)
                        {
                            break (outcome, error);
                        }
                    }
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error } => {
                    info!("Outgoing connection error to {:?}: {:?}", peer_id, error);
                    if peer_id == Some(self.remote_id) {
                        let is_connected = swarm.is_connected(&self.remote_id);
                        if let ControlFlow::Break((outcome, error)) =
                            self.handle_connection_error(is_connected)
                        {
                            break (outcome, error);
                        }
                    }
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    endpoint,
                    num_established,
                    ..
                } => {
                    info!("Connection to {:?} via {:?} closed", peer_id, endpoint);
                    if peer_id == self.remote_id {
                        let remote_addr = endpoint.get_remote_address().clone().to_vec();
                        self.request
                            .open_multi_addresses
                            .retain(|a| a != &remote_addr);
                        if num_established == 0 {
                            let error =
                                Some("connection closed without a successful hole-punch".into());
                            break (grpc::HolePunchOutcome::Failed, error);
                        }
                    }
                }
                _ => {}
            }
        };
        self.request.outcome = outcome.into();
        self.request.error = error;
        self.request.ended_at = unix_time_now();
        info!(
            "Finished whole hole punching process: attempts: {:?}, outcome: {:?}, error: {:?}",
            self.request.hole_punch_attempts.len(),
            self.request.outcome,
            self.request.error
        );
        self.request
    }

    fn handle_dcutr_event(
        &mut self,
        event: dcutr::behaviour::Event,
    ) -> ControlFlow<(grpc::HolePunchOutcome, Option<String>)> {
        match event {
            dcutr::behaviour::Event::RemoteInitiatedDirectConnectionUpgrade { .. } => {
                self.active_holepunch_attempt = Some(HolePunchAttemptState {
                    opened_at: self.request.connect_ended_at,
                    started_at: unix_time_now(),
                });
            }
            dcutr::behaviour::Event::DirectConnectionUpgradeSucceeded { .. } => {
                if let Some(attempt) = self.active_holepunch_attempt.take() {
                    let resolved = attempt.resolve(grpc::HolePunchAttemptOutcome::Success, None);
                    self.request.hole_punch_attempts.push(resolved);
                }
                return ControlFlow::Break((grpc::HolePunchOutcome::Success, None));
            }
            dcutr::behaviour::Event::DirectConnectionUpgradeFailed { error, .. } => {
                if let Some(attempt) = self.active_holepunch_attempt.take() {
                    let (attempt_outcome, attempt_error) = match error {
                        UpgradeError::Dial => (
                            grpc::HolePunchAttemptOutcome::Failed,
                            "failed to establish a direct connection",
                        ),
                        UpgradeError::Handler(ConnectionHandlerUpgrErr::Timeout) => (
                            grpc::HolePunchAttemptOutcome::Timeout,
                            "hole-punch timed out",
                        ),
                        UpgradeError::Handler(ConnectionHandlerUpgrErr::Timer) => {
                            (grpc::HolePunchAttemptOutcome::Timeout, "timer error")
                        }
                        UpgradeError::Handler(ConnectionHandlerUpgrErr::Upgrade(_)) => (
                            grpc::HolePunchAttemptOutcome::ProtocolError,
                            "protocol error",
                        ),
                    };
                    let resolved = attempt.resolve(attempt_outcome, Some(attempt_error.into()));
                    self.request.hole_punch_attempts.push(resolved);
                }
                let error = Some("none of the 3 attempts succeeded".into());
                return ControlFlow::Break((grpc::HolePunchOutcome::Failed, error));
            }
            dcutr::behaviour::Event::InitiatedDirectConnectionUpgrade { .. } => {
                unreachable!()
            }
        }
        ControlFlow::Continue(())
    }

    fn handle_established_connection(
        &mut self,
        endpoint: ConnectedPoint,
        num_established: NonZeroU32,
    ) -> ControlFlow<(grpc::HolePunchOutcome, Option<String>)> {
        if num_established == NonZeroU32::new(1).expect("1 != 0") {
            self.request.connect_ended_at = unix_time_now();
        }

        self.request
            .open_multi_addresses
            .push(endpoint.get_remote_address().to_vec());

        if !endpoint.is_relayed() {
            self.request.has_direct_conns = true;
        }

        if self.active_holepunch_attempt.is_none() && !endpoint.is_relayed() {
            // Reverse-dial succeeded.
            return ControlFlow::Break((grpc::HolePunchOutcome::ConnectionReversed, None));
        }

        ControlFlow::Continue(())
    }

    fn handle_connection_error(
        &mut self,
        is_connected: bool,
    ) -> ControlFlow<(grpc::HolePunchOutcome, Option<String>)> {
        if !is_connected {
            // Initial connection to the remote failed.
            self.request.connect_ended_at = unix_time_now();
            let error = Some("Error connecting to remote peer via relay.".to_string());
            return ControlFlow::Break((grpc::HolePunchOutcome::NoConnection, error));
        }

        if let Some(attempt) = self.active_holepunch_attempt.take() {
            // Hole punch attempt failed. Up to 3 attempts may fail before the whole hole-punch
            // is considered as failed.
            let attempt_outcome = grpc::HolePunchAttemptOutcome::Failed;
            let attempt_error = "failed to establish a direct connection";
            let resolved = attempt.resolve(attempt_outcome, Some(attempt_error.into()));
            self.request.hole_punch_attempts.push(resolved);
        }
        ControlFlow::Continue(())
    }
}

fn unix_time_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .try_into()
        .unwrap()
}

struct HolePunchAttemptState {
    opened_at: u64,
    started_at: u64,
}

impl HolePunchAttemptState {
    fn resolve(
        self,
        attempt_outcome: grpc::HolePunchAttemptOutcome,
        attempt_error: Option<String>,
    ) -> grpc::HolePunchAttempt {
        let ended_at = unix_time_now();
        info!(
            "Finished hole punching attempt with outcome: {:?}, error: {:?}",
            attempt_outcome, attempt_error
        );
        grpc::HolePunchAttempt {
            opened_at: self.opened_at,
            started_at: Some(self.started_at),
            ended_at,
            start_rtt: None,
            elapsed_time: (ended_at - self.started_at) as f32 / 1000f32,
            outcome: attempt_outcome.into(),
            error: attempt_error,
            direct_dial_error: None,
        }
    }
}

fn generate_ed25519(secret_key_seed: u8) -> identity::Keypair {
    let mut bytes = [0u8; 32];
    bytes[0] = secret_key_seed;

    let secret_key = identity::ed25519::SecretKey::from_bytes(&mut bytes)
        .expect("this returns `Err` only if the length is wrong; the length is correct; qed");
    identity::Keypair::Ed25519(secret_key.into())
}

#[derive(libp2p::NetworkBehaviour)]
#[behaviour(out_event = "Event", event_process = false)]
struct Behaviour {
    relay: Client,
    ping: Ping,
    identify: Identify,
    dcutr: dcutr::behaviour::Behaviour,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Event {
    Ping(PingEvent),
    Identify(IdentifyEvent),
    Relay(client::Event),
    Dcutr(dcutr::behaviour::Event),
}

impl From<PingEvent> for Event {
    fn from(e: PingEvent) -> Self {
        Event::Ping(e)
    }
}

impl From<IdentifyEvent> for Event {
    fn from(e: IdentifyEvent) -> Self {
        Event::Identify(e)
    }
}

impl From<client::Event> for Event {
    fn from(e: client::Event) -> Self {
        Event::Relay(e)
    }
}

impl From<dcutr::behaviour::Event> for Event {
    fn from(e: dcutr::behaviour::Event) -> Self {
        Event::Dcutr(e)
    }
}

/// Dummy TLS Server verifier that approves all server certificates.
struct DummyVerifyAll;

impl ServerCertVerifier for DummyVerifyAll {
    fn verify_server_cert(
        &self,
        _: &rustls::Certificate,
        _: &[rustls::Certificate],
        _: &rustls::ServerName,
        _: &mut dyn Iterator<Item = &[u8]>,
        _: &[u8],
        _: std::time::SystemTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::Certificate,
        _: &rustls::internal::msgs::handshake::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::Certificate,
        _: &rustls::internal::msgs::handshake::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
}

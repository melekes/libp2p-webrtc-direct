use anyhow::Result;
use async_trait::async_trait;
use futures::future::FutureExt;
use futures::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use futures::stream::StreamExt;
use libp2p_core::identity;
use libp2p_core::upgrade;
use libp2p_request_response::{
    ProtocolName, ProtocolSupport, RequestResponse, RequestResponseCodec, RequestResponseConfig,
    RequestResponseEvent, RequestResponseMessage,
};
use libp2p_swarm::{Swarm, SwarmEvent};
use libp2p_webrtc_direct::transport::WebRTCDirectTransport;
use log::trace;
use rand::RngCore;
use rcgen::KeyPair;
use tokio_crate as tokio;

use std::{io, iter};

use webrtc::peer_connection::certificate::RTCCertificate;

fn generate_certificate() -> RTCCertificate {
    let kp = KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256).expect("key pair");
    RTCCertificate::from_key_pair(kp).expect("certificate")
}

fn generate_tls_keypair() -> identity::Keypair {
    identity::Keypair::generate_ed25519()
}

async fn create_swarm() -> Result<Swarm<RequestResponse<PingCodec>>> {
    let cert = generate_certificate();
    let keypair = generate_tls_keypair();
    let peer_id = keypair.public().to_peer_id();
    let transport = WebRTCDirectTransport::new(cert, keypair, "127.0.0.1:0")
        .await?
        .boxed();

    let protocols = iter::once((PingProtocol(), ProtocolSupport::Full));
    let cfg = RequestResponseConfig::default();
    let behaviour = RequestResponse::new(PingCodec(), protocols, cfg);
    trace!("{}", peer_id);
    Ok(Swarm::new(transport, behaviour, peer_id))
}

#[tokio::test]
async fn smoke() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();
    log_panics::init();
    let mut rng = rand::thread_rng();

    let mut a = create_swarm().await?;
    let mut b = create_swarm().await?;

    Swarm::listen_on(&mut a, "/ip4/127.0.0.1/udp/0".parse()?)?;

    let addr = match a.next().await {
        Some(SwarmEvent::NewListenAddr { address, .. }) => address,
        e => panic!("{:?}", e),
    };

    let mut data = vec![0; 4096 * 10];
    rng.fill_bytes(&mut data);

    b.behaviour_mut()
        .add_address(&Swarm::local_peer_id(&a), addr);
    b.behaviour_mut()
        .send_request(&Swarm::local_peer_id(&a), Ping(data.clone()));

    match b.next().await {
        Some(SwarmEvent::Dialing(_)) => {},
        e => panic!("{:?}", e),
    }

    match a.next().await {
        Some(SwarmEvent::IncomingConnection { .. }) => {},
        e => panic!("{:?}", e),
    };

    match b.next().await {
        Some(SwarmEvent::ConnectionEstablished { .. }) => {},
        e => panic!("{:?}", e),
    };

    match a.next().await {
        Some(SwarmEvent::ConnectionEstablished { .. }) => {},
        e => panic!("{:?}", e),
    };

    assert!(b.next().now_or_never().is_none());

    match a.next().await {
        Some(SwarmEvent::Behaviour(RequestResponseEvent::Message {
            message:
                RequestResponseMessage::Request {
                    request: Ping(ping),
                    channel,
                    ..
                },
            ..
        })) => {
            a.behaviour_mut()
                .send_response(channel, Pong(ping))
                .unwrap();
        },
        e => panic!("{:?}", e),
    }

    match a.next().await {
        Some(SwarmEvent::Behaviour(RequestResponseEvent::ResponseSent { .. })) => {},
        e => panic!("{:?}", e),
    }

    match b.next().await {
        Some(SwarmEvent::Behaviour(RequestResponseEvent::Message {
            message:
                RequestResponseMessage::Response {
                    response: Pong(pong),
                    ..
                },
            ..
        })) => assert_eq!(data, pong),
        e => panic!("{:?}", e),
    }

    a.behaviour_mut().send_request(
        &Swarm::local_peer_id(&b),
        Ping(b"another substream".to_vec()),
    );

    assert!(a.next().now_or_never().is_none());

    match b.next().await {
        Some(SwarmEvent::Behaviour(RequestResponseEvent::Message {
            message:
                RequestResponseMessage::Request {
                    request: Ping(data),
                    channel,
                    ..
                },
            ..
        })) => {
            b.behaviour_mut()
                .send_response(channel, Pong(data))
                .unwrap();
        },
        e => panic!("{:?}", e),
    }

    match b.next().await {
        Some(SwarmEvent::Behaviour(RequestResponseEvent::ResponseSent { .. })) => {},
        e => panic!("{:?}", e),
    }

    match a.next().await {
        Some(SwarmEvent::Behaviour(RequestResponseEvent::Message {
            message:
                RequestResponseMessage::Response {
                    response: Pong(data),
                    ..
                },
            ..
        })) => assert_eq!(data, b"another substream".to_vec()),
        e => panic!("{:?}", e),
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct PingProtocol();

#[derive(Clone)]
struct PingCodec();

#[derive(Debug, Clone, PartialEq, Eq)]
struct Ping(Vec<u8>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pong(Vec<u8>);

impl ProtocolName for PingProtocol {
    fn protocol_name(&self) -> &[u8] {
        "/ping/1".as_bytes()
    }
}

#[async_trait]
impl RequestResponseCodec for PingCodec {
    type Protocol = PingProtocol;
    type Request = Ping;
    type Response = Pong;

    async fn read_request<T>(&mut self, _: &PingProtocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        upgrade::read_length_prefixed(io, 4096 * 10)
            .map(|res| match res {
                Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
                Ok(vec) if vec.is_empty() => Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(vec) => Ok(Ping(vec)),
            })
            .await
    }

    async fn read_response<T>(&mut self, _: &PingProtocol, io: &mut T) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        upgrade::read_length_prefixed(io, 4096 * 10)
            .map(|res| match res {
                Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
                Ok(vec) if vec.is_empty() => Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(vec) => Ok(Pong(vec)),
            })
            .await
    }

    async fn write_request<T>(
        &mut self,
        _: &PingProtocol,
        io: &mut T,
        Ping(data): Ping,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        upgrade::write_length_prefixed(io, data).await?;
        io.close().await?;
        Ok(())
    }

    async fn write_response<T>(
        &mut self,
        _: &PingProtocol,
        io: &mut T,
        Pong(data): Pong,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        upgrade::write_length_prefixed(io, data).await?;
        io.close().await?;
        Ok(())
    }
}

#[tokio::test]
async fn dial_failure() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();
    log_panics::init();

    let mut a = create_swarm().await?;
    let mut b = create_swarm().await?;

    Swarm::listen_on(&mut a, "/ip4/127.0.0.1/udp/0".parse()?)?;

    let addr = match a.next().await {
        Some(SwarmEvent::NewListenAddr { address, .. }) => address,
        e => panic!("{:?}", e),
    };
    let a_peer_id = &Swarm::local_peer_id(&a).clone();
    drop(a); // stop a swarm so b can never reach it

    b.behaviour_mut().add_address(a_peer_id, addr);
    b.behaviour_mut()
        .send_request(a_peer_id, Ping(b"hello world".to_vec()));

    match b.next().await {
        Some(SwarmEvent::Dialing(_)) => {},
        e => panic!("{:?}", e),
    }

    match b.next().await {
        Some(SwarmEvent::OutgoingConnectionError { .. }) => {},
        e => panic!("{:?}", e),
    };

    match b.next().await {
        Some(SwarmEvent::Behaviour(RequestResponseEvent::OutboundFailure { .. })) => {},
        e => panic!("{:?}", e),
    };

    Ok(())
}

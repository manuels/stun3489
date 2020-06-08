pub mod codec;

pub use crate::codec::StunCodec;

use std::io::Error;
use std::io::ErrorKind;
use std::net::SocketAddr;

use log::{debug, info};
use rand::SeedableRng;
use rand::rngs::SmallRng;
use rand::Rng;

use futures::prelude::*;

use crate::codec::BindRequest;
use crate::codec::ChangeRequest;
use crate::codec::Request;
use crate::codec::Response;

pub const NETWORK_UNREACHABLE: i32 = 101;

pub struct Stun3489<I, O>
    where
        I: TryStream<Ok=((u64, Response), SocketAddr), Error=std::io::Error> + Unpin,
        O: Sink<((u64, Request), SocketAddr), Error=std::io::Error> + Unpin,
{
    rng: SmallRng,
    stream: I,
    sink: O,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Connectivity {
    OpenInternet(SocketAddr),
    FullConeNat(SocketAddr),
    SymmetricNat,
    RestrictedPortNat(SocketAddr),
    RestrictedConeNat(SocketAddr),
    SymmetricFirewall(SocketAddr),
}

impl Into<Option<SocketAddr>> for Connectivity {
    fn into(self) -> Option<SocketAddr> {
        match self {
            Connectivity::OpenInternet(addr) => Some(addr),
            Connectivity::FullConeNat(addr) => Some(addr),
            Connectivity::SymmetricNat => None,
            Connectivity::RestrictedPortNat(addr) => Some(addr),
            Connectivity::RestrictedConeNat(addr) => Some(addr),
            Connectivity::SymmetricFirewall(addr) => Some(addr),
        }
    }
}

impl<I, O> Stun3489<I, O>
    where
        I: TryStream<Ok=((u64, Response), SocketAddr), Error=std::io::Error> + Unpin,
        O: Sink<((u64, Request), SocketAddr), Error=std::io::Error> + Unpin,
{
    pub fn new(sink: O, stream: I) -> Stun3489<I, O> {
        Stun3489 {
            rng: SmallRng::from_entropy(),
            sink,
            stream,
        }
    }

    pub fn into_inner(self) -> (O, I) {
        (self.sink, self.stream)
    }

    pub async fn check(
        &mut self,
        bind_addr: SocketAddr,
        stun_server: SocketAddr,
    ) -> Result<Connectivity, std::io::Error> {
        let resp = self.change_request(stun_server, ChangeRequest::None).await?;
        if let Some(Response::Bind(resp)) = resp {
            let public_addr = resp.mapped_address;

            if bind_addr.ip() == public_addr.ip() {
                debug!(
                    "No NAT. Public IP ({}) == Bind IP ({})",
                    bind_addr.ip(),
                    public_addr.ip()
                );
                let resp = self.change_request(stun_server, ChangeRequest::IpAndPort).await?;
                if resp.is_some() {
                    info!("OpenInternet: {}", public_addr);
                    return Ok(Connectivity::OpenInternet(public_addr));
                } else {
                    info!("SymmetricFirewall: {}", public_addr);
                    return Ok(Connectivity::SymmetricFirewall(public_addr));
                }
            }
            debug!("Public IP ({}) != Bind IP ({})", bind_addr.ip(), public_addr.ip());

            // NAT detected
            let resp = self.change_request(stun_server, ChangeRequest::IpAndPort).await?;
            if resp.is_some() {
                info!("FullConeNat: {}", public_addr);
                return Ok(Connectivity::FullConeNat(public_addr));
            }

            debug!("No respone from different IP and Port");
            let resp = self.change_request(stun_server, ChangeRequest::Port).await?;
            if let Some(Response::Bind(resp)) = resp {
                if resp.mapped_address.ip() != public_addr.ip() {
                    info!("SymmetricNat");
                    return Ok(Connectivity::SymmetricNat);
                }

                let resp = self.change_request(stun_server, ChangeRequest::Port).await?;
                if resp.is_some() {
                    info!("RestrictedConeNat: {}", public_addr);
                    Ok(Connectivity::RestrictedConeNat(public_addr))
                } else {
                    info!("RestrictedPortNat: {}", public_addr);
                    Ok(Connectivity::RestrictedPortNat(public_addr))
                }
            } else {
                let msg = format!("Expected Some(BindResponse) but got {:?} instead!", resp);
                Err(Error::new(ErrorKind::InvalidData, msg))
            }
        } else {
            Err(Error::from_raw_os_error(NETWORK_UNREACHABLE))
        }
    }

    async fn change_request(
        &mut self,
        stun_server: SocketAddr,
        req: ChangeRequest,
    ) -> Result<Option<Response>, std::io::Error> {
        let req = codec::Request::Bind(BindRequest {
            change_request: req,
            ..Default::default()
        });

        self.send_request(stun_server, req).await
    }

    async fn send_request(
        &mut self,
        stun_server: SocketAddr,
        req: Request,
    ) -> Result<Option<Response>, std::io::Error> {
        let id: u64 = self.rng.gen();

        self.sink.send(((id, req), stun_server)).await?;

        loop {
            match self.stream.try_next().await {
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => return Ok(None),
                Err(e) => return Err(e),
                Ok(Some(((actual_id, resp), _src))) => {
                    if actual_id == id {
                        return Ok(Some(resp));
                    }
                }
                Ok(None) => return Ok(None),
            }
        }
    }
}

#[test]
fn test_connectivity() {
    use std::time::Duration;
    use std::net::ToSocketAddrs;

    use async_std::prelude::*;
    use async_std::task::{block_on, spawn};
    use async_std::net::UdpSocket;
    use std::os::unix::io::FromRawFd;
    use std::os::unix::io::IntoRawFd;

    env_logger::init();

    block_on(async {
        let mut addrs_iter = "stun.wtfismyip.com:3478".to_socket_addrs().unwrap();
        let server = addrs_iter.next().unwrap();

        let bind_addr = SocketAddr::from(([0, 0, 0, 0], 0));
        let sock = UdpSocket::bind(&bind_addr).await.unwrap();

        let fd1 = sock.into_raw_fd();
        let fd2 = nix::unistd::dup(fd1).expect("dup() failed");
        let sock1 = unsafe { UdpSocket::from_raw_fd(fd1) };
        let sock2 = unsafe { UdpSocket::from_raw_fd(fd2) };

        let (mut tx1, rx1) = futures::channel::mpsc::unbounded();
        let (tx2, mut rx2) = futures::channel::mpsc::unbounded();

        spawn(async move {
            let mut buf = vec![0u8; 4 * 1024];
            while let Ok((n, peer)) = sock1.recv_from(&mut buf).await {
                if let Some(response) = crate::codec::StunCodec::decode_const(&buf[..n]).unwrap() {
                    tx1.send((response, peer)).await.expect("unable to forward received bytes");
                }
            }
        });

        spawn(async move {
            let mut buf = bytes::BytesMut::with_capacity(4 * 1024);
            while let Some(((id, request), peer)) = futures::StreamExt::next(&mut rx2).await {
                crate::codec::StunCodec::encode((id, request), &mut buf).expect("Cannot encode STUN request");
                sock2.send_to(&buf[..], peer).await.expect("unable to forward received bytes");
                buf.clear();
            }
        });

        let tx2 = tx2.sink_map_err(|_| Error::new(ErrorKind::Other, "SendError"));

        let rx1 = rx1.timeout(Duration::from_secs(3))
            .map_err(|_| Error::new(ErrorKind::TimedOut, "Timed out"));

        let mut stun = crate::Stun3489::new(tx2, rx1);
        let job = stun.check(bind_addr, server);
        println!("{:?}", job.await);
    })
}

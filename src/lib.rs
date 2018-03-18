#![feature(pin)]
#![feature(await_macro, async_await, futures_api)]

extern crate byteorder;
extern crate rand;
extern crate ring;
extern crate futures;
#[macro_use] extern crate log;
extern crate env_logger;
extern crate bytes;

pub mod codec;
pub use crate::codec::StunCodec;

use std::io::Error;
use std::io::ErrorKind;
use std::net::SocketAddr;

use rand::Rng;
use rand::FromEntropy;
use rand::rngs::SmallRng;

use tokio::prelude::*;

use crate::codec::Request;
use crate::codec::Response;
use crate::codec::BindRequest;
use crate::codec::ChangeRequest;

pub const NETWORK_UNREACHABLE:i32 = 101;

pub struct Stun3489<I, O>
where I: Stream<Item=((u64, Response), SocketAddr), Error=std::io::Error>,
      O: Sink<SinkItem=((u64, Request), SocketAddr), SinkError=std::io::Error>
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
where I: Stream<Item=((u64, Response), SocketAddr), Error=std::io::Error> + std::marker::Unpin,
      O: Sink<SinkItem=((u64, Request), SocketAddr), SinkError=std::io::Error> + std::marker::Unpin
{
    /// `stream` should be a tokio_timer::TimeoutStream
    pub fn new(sink: O, stream: I) -> Stun3489<I, O> {
        Stun3489 {
            rng: SmallRng::from_entropy(),
            sink, stream
        }
    }

    pub fn into_inner(self) -> (O, I) {
        (self.sink, self.stream)
    }

    pub async fn check(&mut self,
            bind_addr: SocketAddr,
            stun_server: SocketAddr)
        -> Result<Connectivity, std::io::Error>
    {
        let resp = await!(self.change_request(stun_server, ChangeRequest::None))?;
        if let Some(Response::Bind(resp)) = resp {
            let public_addr = resp.mapped_address;
            let public_ip = public_addr.ip();

            if bind_addr.ip() == public_ip {
                debug!("No NAT. Public IP ({}) == Bind IP ({})", bind_addr.ip(), public_ip);
                let resp = await!(self.change_request(stun_server, ChangeRequest::IpAndPort))?;
                if resp.is_some() {
                    info!("OpenInternet: {}", public_addr);
                    return Ok(Connectivity::OpenInternet(public_addr));
                } else {
                    info!("SymmetricFirewall: {}", public_addr);
                    return Ok(Connectivity::SymmetricFirewall(public_addr));
                }
            }
            debug!("Public IP ({}) != Bind IP ({})", bind_addr.ip(), public_ip);

            // NAT detected
            let resp = await!(self.change_request(stun_server, ChangeRequest::IpAndPort))?;
            if resp.is_some() {
                info!("FullConeNat: {}", public_addr);
                return Ok(Connectivity::FullConeNat(public_addr));
            }

            debug!("No respone from different IP and Port");
            let resp = await!(self.change_request(stun_server, ChangeRequest::Port))?;
            if let Some(Response::Bind(resp)) = resp {
                if resp.mapped_address.ip() != public_ip {
                    info!("SymmetricNat");
                    return Ok(Connectivity::SymmetricNat);
                }

                let resp = await!(self.change_request(stun_server, ChangeRequest::Port))?;
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

    async fn change_request(&mut self, stun_server: SocketAddr, req: ChangeRequest)
        -> Result<Option<Response>, std::io::Error>
    {
        let req = codec::Request::Bind(BindRequest {
            change_request: req,
            ..Default::default()
        });

        await!(self.send_request(stun_server, req))
    }

    async fn send_request(&mut self, stun_server: SocketAddr, req: Request)
        -> Result<Option<Response>, std::io::Error>
    {
        let id: u64 = self.rng.gen();

        await!(self.sink.send_async(((id, req), stun_server)))?;

        while let Some(res) = await!(self.stream.next()) {
            match res {
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => return Ok(None),
                Err(e) => return Err(e),
                Ok(((actual_id, resp), _src)) => {
                    if actual_id == id {
                        return Ok(Some(resp));
                    }
                }
            };
        }

        Ok(None)
    }
}

#[test]
fn test_connectivity() {
    use std::time::Duration;
    use std::net::ToSocketAddrs;

    use tokio::net::UdpSocket;
    use tokio::net::UdpFramed;

    tokio::run_async(async {
        let bind_addr = SocketAddr::from(([0, 0, 0, 0], 0));
        let sock = UdpSocket::bind(&bind_addr).unwrap();
        let (sink, stream) = UdpFramed::new(sock, StunCodec).split();

        let stream = stream.timeout(Duration::from_secs(1));
        let stream = stream.map_err(|e| e.into_inner().unwrap_or_else(||
            Error::new(ErrorKind::TimedOut, ""))
        );

        let mut stun = crate::Stun3489::new(sink, stream);

        let mut addrs_iter = "stun.wtfismyip.com:3478".to_socket_addrs().unwrap();
        let server = addrs_iter.next().unwrap();

        let job = stun.check(bind_addr, server);
        println!("{:?}", await!(job));
    })
}

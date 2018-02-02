#![feature(conservative_impl_trait, generators, proc_macro, nll)]

extern crate byteorder;
extern crate rand;
extern crate ring;
extern crate tokio_core;
extern crate tokio_timer;
extern crate futures_await;
#[macro_use] extern crate log;
extern crate env_logger;

pub mod codec;

use std::io::Error;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;

use rand::Rng;

use futures_await as futures;
use futures::prelude::*;

use codec::Request;
use codec::Response;
use codec::BindRequest;
use codec::ChangeRequest;

pub const NETWORK_UNREACHABLE:i32 = 101;

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

type O = Box<Sink<SinkItem=(SocketAddr, u64, Request), SinkError=std::io::Error>>;
type I = Box<Stream<Item=(u64, Response), Error=std::io::Error>>;

pub fn codec<II,OO>(sink: OO, stream: II)
    -> (O, I)
    where OO: Sink<SinkItem=(SocketAddr, Vec<u8>), SinkError=std::io::Error> + 'static,
          II: Stream<Item=(SocketAddr, Vec<u8>), Error=std::io::Error> + 'static,
{
    let stream = stream.filter_map(|(_src, msg)| codec::decode(&msg).ok());
    let sink = sink.with(|info| {
        let mut data = vec![];
        let dst = codec::encode(info, &mut data);
        Ok((dst, data))
    });

    (Box::new(sink), Box::new(stream))
}

/// `stream` should be a tokio_timer::TimeoutStream
#[async]
pub fn connectivity<B,S>(
        sink: O,
        stream: I,
        bind_addr: B,
        stun_server: S)
    -> Result<((O, I), Connectivity), ((O, I), std::io::Error)>
    where B: ToSocketAddrs + 'static,
          S: ToSocketAddrs + 'static,
{
    let bind_addr = bind_addr.to_socket_addrs().unwrap().next().unwrap();
    let stun_server = stun_server.to_socket_addrs().unwrap().next().unwrap();

    let io = (sink, stream);
    let (io, resp) = await!(change_request(io, stun_server, ChangeRequest::None))?;
    if let Some(Response::Bind(resp)) = resp {
        let public_addr = resp.mapped_address;
        let public_ip = public_addr.ip();

        if bind_addr.ip() == public_ip {
            debug!("No NAT. Public IP ({}) == Bind IP ({})", bind_addr.ip(), public_ip);
            let (io, resp) = await!(change_request(io, stun_server, ChangeRequest::IpAndPort))?;
            if resp.is_some() {
                info!("OpenInternet: {}", public_addr);
                return Ok((io, Connectivity::OpenInternet(public_addr)));
            } else {
                info!("SymmetricFirewall: {}", public_addr);
                return Ok((io, Connectivity::SymmetricFirewall(public_addr)));
            }
        }
        debug!("Public IP ({}) != Bind IP ({})", bind_addr.ip(), public_ip);

        // NAT detected
        let (io, resp) = await!(change_request(io, stun_server, ChangeRequest::IpAndPort))?;
        if resp.is_some() {
            info!("FullConeNat: {}", public_addr);
            return Ok((io, Connectivity::FullConeNat(public_addr)));
        }

        debug!("No respone from different IP and Port");
        let (io, resp) = await!(change_request(io, stun_server, ChangeRequest::Port))?;
        if let Some(Response::Bind(resp)) = resp {
            if resp.mapped_address.ip() != public_ip {
                info!("SymmetricNat");
                return Ok((io, Connectivity::SymmetricNat));
            }

            let (io, resp) = await!(change_request(io, stun_server, ChangeRequest::Port))?;
            if resp.is_some() {
                info!("RestrictedConeNat: {}", public_addr);
                Ok((io, Connectivity::RestrictedConeNat(public_addr)))
            } else {
                info!("RestrictedPortNat: {}", public_addr);
                Ok((io, Connectivity::RestrictedPortNat(public_addr)))
            }
        } else {
            let msg = format!("Expected Some(BindResponse) but got {:?} instead!", resp);
            Err((io, Error::new(ErrorKind::InvalidData, msg)))
        }
    } else {
        Err((io, Error::from_raw_os_error(NETWORK_UNREACHABLE)))
    }
}

#[async]
fn change_request(io: (O, I), stun_server: SocketAddr, req: ChangeRequest)
    -> Result<((O, I), Option<Response>), ((O, I), std::io::Error)>
{
    let req = codec::Request::Bind(BindRequest {
        change_request: req,
        ..BindRequest::default()
    });

    await!(send_request(io, stun_server, req))
}

#[async]
fn send_request(io: (O, I), stun_server: SocketAddr, req: Request)
    -> Result<((O, I), Option<Response>), ((O, I), std::io::Error)>
{
    let (mut sink, mut stream) = io;

    let mut rng = rand::thread_rng();
    let id: u64 = rng.gen();

    let sink = match await!(sink.send((stun_server, id, req))) {
        Ok(sink) => sink,
        Err(_) => unimplemented!(), // TODO
    };

    let stream = stream.skip_while(move |&(actual_id, ref _resp)| Ok(actual_id != id) );

    match await!(stream.into_future()) {
        Ok((Some((_id, resp)), t)) => {
            let stream = Box::new(t);
            Ok(((sink, stream), Some(resp)))
        },
        Ok((None, t)) => {
            let stream = Box::new(t);
            Ok(((sink, stream), None))
        },
        Err((e, t)) => {
            debug!("Err = {:?}", e);
            let stream = Box::new(t);

            if e.kind() == std::io::ErrorKind::TimedOut {
                Ok(((sink, stream), None))
            } else {
                Err(((sink, stream), e))
            }
        }
    }
}

#[test]
fn test_connectivity() {
    use tokio_core::reactor::Core;
    use tokio_core::net::UdpSocket;
    use std::time::Duration;
    use tokio_timer::Timer;

    use codec::StunCodec;

    let bind_addr = SocketAddr::from(([0, 0, 0, 0], 0));
    let server = SocketAddr::from(([158,69,26,138], 3478)); // stun.wtfismyip.com

    let mut core = Core::new().unwrap();
    let handle = core.handle();

    let sock = UdpSocket::bind(&bind_addr, &handle).unwrap();
    let (sink, stream) = sock.framed(StunCodec).split();

    let stream = Timer::default().timeout_stream(stream, Duration::from_secs(1));
    let job = connectivity(Box::new(sink), Box::new(stream), bind_addr,
        server);

    match core.run(job) {
        Ok((_, _conn)) => (),
        Err((_, e)) => Err(e).unwrap()
    }
}

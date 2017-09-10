#![feature(proc_macro, conservative_impl_trait, generators)]

extern crate byteorder;
extern crate futures_await as futures;
extern crate tokio_core;
extern crate ring;
extern crate rand;
#[macro_use] extern crate log;

mod codec;
mod request;

use std::io;
use std::io::Error;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::Duration;

use tokio_core::net::UdpCodec;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;
use tokio_core::reactor::Timeout;

use futures::prelude::*;
use futures::stream::Stream;
use futures::sink::Sink;

use futures::Future;

use codec::Response;
use codec::BindRequest;
use codec::ChangeRequest;

use request::Request;

pub const NETWORK_UNREACHABLE:i32 = 101;

#[derive(Debug, PartialEq)]
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

struct Codec;

impl UdpCodec for Codec {
    type In = (Vec<u8>, SocketAddr);
    type Out = (Vec<u8>, SocketAddr);

    fn decode(&mut self, src: &SocketAddr, buf: &[u8]) -> io::Result<Self::In> {
        Ok((buf.to_vec(), *src))
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> SocketAddr {
        buf.append(&mut msg.0.clone());
        msg.1
    }
}

#[async]
pub fn stun3489(bind_addr:   SocketAddr,
                stun_server: SocketAddr,
                handle:      Handle,
                timeout:     Duration)
    -> io::Result<Connectivity>
{
    let sock = UdpSocket::bind(&bind_addr.clone(), &handle)?;
    let bind_addr = sock.local_addr()?;
    let (sink, stream) = sock.framed(Codec).split();

    await!(stun3489_generic(Box::new(sink), stream.boxed(), bind_addr, stun_server, handle, timeout)
        .map(|(_,_,c)| c)
        .map_err(|(_,_,e)| e))
}

#[async]
pub fn stun3489_generic<S,T>(sink:        S,
                             stream:      T,
                             bind_addr:   SocketAddr,
                             stun_server: SocketAddr,
                             handle:      Handle,
                             timeout:     Duration)
    -> Result<(S,T, Connectivity), (S,T,io::Error)>
    where S: Sink<SinkItem=(Vec<u8>, SocketAddr), SinkError=io::Error> + 'static,
          T: Stream<Item=(Vec<u8>, SocketAddr), Error=io::Error> + 'static
{
    let req = codec::Request::Bind(BindRequest::default());
    let req_same_ip_same_port = req.clone();
    let req_same_ip_diff_port = codec::Request::Bind(BindRequest { change_request: Some(ChangeRequest::Port), ..BindRequest::default() });
    let req_diff_ip_diff_port = codec::Request::Bind(BindRequest { change_request: Some(ChangeRequest::IpAndPort), ..BindRequest::default() });

    let halt = Timeout::new(timeout, &handle);
    let handle = handle.clone();

    let request1 = Request::new(sink, stream, stun_server.clone(), req, halt);
    let (sink, stream, response) = await!(request1)?;

    if let Some(Response::Bind(response)) = response {
        let halt = Timeout::new(timeout, &handle);
        let request2 = Request::new(sink, stream, stun_server.clone(), req_diff_ip_diff_port, halt);

        let public_addr = response.mapped_address;
        if bind_addr.ip() == public_addr.ip() {
            // No NAT
            let (sink, stream, response) = await!(request2)?;

            if response.is_some() {
                return Ok((sink, stream, Connectivity::OpenInternet(public_addr)));
            } else {
                return Ok((sink, stream, Connectivity::SymmetricFirewall(public_addr)));
            }
        }

        // NAT detected
        let (sink, stream, response) = await!(request2)?;
        if response.is_some() {
            return Ok((sink, stream, Connectivity::FullConeNat(public_addr)));
        }

        let halt = Timeout::new(timeout, &handle);
        let request3 = Request::new(sink, stream, stun_server.clone(), req_same_ip_same_port, halt);
        let (sink, stream, response) = await!(request3)?;

        if let Some(Response::Bind(response)) = response {
            if public_addr.ip() != response.mapped_address.ip() {
                return Ok((sink, stream, Connectivity::SymmetricNat));
            }

            let halt = Timeout::new(timeout, &handle);
            let request4 = Request::new(sink, stream, stun_server.clone(), req_same_ip_diff_port, halt);
            let (sink, stream, response) = await!(request4)?;

            if response.is_some() {
                Ok((sink, stream, Connectivity::RestrictedConeNat(public_addr)))
            } else {
                Ok((sink, stream, Connectivity::RestrictedPortNat(public_addr)))
            }
        } else {
            let msg = format!("Did not receive Some(BindResponse) but got {:?} instead!", response);
            let e = Error::new(ErrorKind::InvalidData, msg);
            Err((sink, stream, e))
        }
    } else {
        let e = Error::from_raw_os_error(NETWORK_UNREACHABLE);
        Err((sink, stream, e))
    }
}


#[cfg(test)]
mod tests {
    use std::time::Duration;
    use tokio_core::reactor::Core;

    use super::stun3489;

    #[test]
    fn it_works() {
        let addr = "0.0.0.0:0".parse().unwrap();
        let server = "217.10.68.152:3478".parse().unwrap(); // stun.sipgate.net
        let timeout = Duration::from_secs(1);

        let mut core = Core::new().unwrap();
        let handle = core.handle();

        let conn = stun3489(addr, server, handle, timeout);
        let result = core.run(conn);
        assert!(result.is_ok());
    }

    #[test]
    fn loopback_gives_error() {
        let addr = "127.0.0.1:0".parse().unwrap();
        let server = "217.10.68.152:3478".parse().unwrap(); // stun.sipgate.net
        let timeout = Duration::from_secs(1);

        let mut core = Core::new().unwrap();
        let handle = core.handle();

        let conn = stun3489(addr, server, handle, timeout);
        let result = core.run(conn);
        println!("{:?}", result);
        assert!(result.is_err());
    }

    #[test]
    fn no_server_gives_blocked() {
        let addr = "0.0.0.0:0".parse().unwrap();
        let server = "240.0.0.1:3478".parse().unwrap();
        let timeout = Duration::from_secs(1);

        let mut core = Core::new().unwrap();
        let handle = core.handle();

        let conn = stun3489(addr, server, handle, timeout);
        let result = core.run(conn);
        println!("{:?}", result);
        assert_eq!(result.err().unwrap().raw_os_error(), Some(::NETWORK_UNREACHABLE));
    }
}


use futures::{Async, Future, Poll};
use multiaddr::{multihash::Multihash, Multiaddr, Protocol, ToMultiaddr};
use secio::PeerId;
use std::{io, marker::PhantomData, net::ToSocketAddrs};

use crate::{error::Error, utils::extract_peer_id};

/// DNS resolver, use on multi-thread tokio runtime
pub struct DNSResolver<T> {
    source_address: Multiaddr,
    peer_id: Option<PeerId>,
    port: u16,
    domain: String,
    phantom: PhantomData<T>,
}

impl<T> DNSResolver<T> {
    /// If address like `/dns4/localhost/tcp/80` or `"/dns6/localhost/tcp/80"`,
    /// it will be return Ok, else Error
    pub fn new(source_address: Multiaddr) -> Result<Self, ()> {
        let mut iter = source_address.iter().peekable();

        let (domain, port) = loop {
            if iter.peek().is_none() {
                break (None, None);
            }
            match iter.peek() {
                Some(Protocol::Dns4(_)) | Some(Protocol::Dns6(_)) => (),
                _ => {
                    let _ = iter.next();
                    continue;
                }
            }

            let proto1 = iter.next().ok_or(())?;
            let proto2 = iter.next().ok_or(())?;

            match (proto1, proto2) {
                (Protocol::Dns4(domain), Protocol::Tcp(port)) => break (Some(domain), Some(port)),
                (Protocol::Dns6(domain), Protocol::Tcp(port)) => break (Some(domain), Some(port)),
                _ => (),
            }
        };

        match (domain, port) {
            (Some(domain), Some(port)) => Ok(DNSResolver {
                peer_id: extract_peer_id(&source_address),
                domain: domain.to_string(),
                source_address,
                port,
                phantom: PhantomData,
            }),
            _ => Err(()),
        }
    }
}

impl<T> Future for DNSResolver<T>
where
    T: Send + ::std::fmt::Debug,
{
    type Item = Multiaddr;
    type Error = (Multiaddr, Error<T>);

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match tokio_threadpool::blocking(|| (self.domain.as_str(), self.port).to_socket_addrs()) {
            Ok(Async::Ready(Ok(mut iter))) => match iter.next() {
                Some(address) => {
                    let mut address = address.to_multiaddr().unwrap();
                    if let Some(peer_id) = self.peer_id.take() {
                        address.append(Protocol::P2p(
                            Multihash::from_bytes(peer_id.as_bytes().to_vec())
                                .expect("Invalid peer id"),
                        ))
                    }
                    Ok(Async::Ready(address))
                }
                None => Err((
                    self.source_address.clone(),
                    Error::DNSResolverError(io::ErrorKind::InvalidData.into()),
                )),
            },
            Ok(Async::Ready(Err(e))) => {
                Err((self.source_address.clone(), Error::DNSResolverError(e)))
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(e) => Err((
                self.source_address.clone(),
                Error::DNSResolverError(io::Error::new(io::ErrorKind::Other, e)),
            )),
        }
    }
}

#[cfg(test)]
mod test {
    use crate::utils::dns::DNSResolver;
    use multiaddr::Multiaddr;

    #[test]
    fn dns_parser() {
        let future: DNSResolver<()> =
            DNSResolver::new("/dns4/localhost/tcp/80".parse().unwrap()).unwrap();
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let addr = rt.block_on(future).unwrap();
        assert_eq!("/ip4/127.0.0.1/tcp/80".parse::<Multiaddr>().unwrap(), addr)
    }
}

//! Nonblocking TCP + `rustls` duplex transport over an already authorized connection.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use mio::{Interest, Registry, Token};
use url::Url;

use crate::http_client::{
    HttpCancel, NetworkScope, SocketRegistration, Transport, connect_transport,
};

enum State {
    Blocking(Transport),
    Plain(mio::net::TcpStream),
    Tls(Box<(rustls::ClientConnection, mio::net::TcpStream)>),
    Empty,
}

/// Owned duplex TCP/TLS transport registered with [`HttpCancel`] for immediate shutdown.
pub struct Wire {
    state: State,
    _registration: SocketRegistration,
}

impl Wire {
    pub fn from_transport(transport: Transport, registration: SocketRegistration) -> Self {
        Self {
            state: State::Blocking(transport),
            _registration: registration,
        }
    }

    pub fn connect_scoped(url: &Url, scope: &NetworkScope, cancel: &HttpCancel) -> Result<Self> {
        let (transport, registration) = connect_transport(
            url,
            scope,
            cancel,
            Some(Instant::now() + Duration::from_secs(10)),
        )?;
        Ok(Self::from_transport(transport, registration))
    }

    pub fn set_nonblocking(&mut self) -> io::Result<()> {
        self.state = match std::mem::replace(&mut self.state, State::Empty) {
            State::Blocking(Transport::Plain(socket)) => {
                let tcp = socket.into_tcp();
                tcp.set_nonblocking(true)?;
                State::Plain(mio::net::TcpStream::from_std(tcp))
            }
            State::Blocking(Transport::Tls(stream)) => {
                let (mut conn, socket) = stream.into_parts();
                let tcp = socket.into_tcp();
                conn.set_buffer_limit(Some(64 * 1024));
                tcp.set_nonblocking(true)?;
                State::Tls(Box::new((conn, mio::net::TcpStream::from_std(tcp))))
            }
            state => state,
        };
        Ok(())
    }

    fn socket(&mut self) -> io::Result<&mut mio::net::TcpStream> {
        match &mut self.state {
            State::Plain(s) => Ok(s),
            State::Tls(s) => Ok(&mut s.1),
            _ => Err(io::ErrorKind::NotConnected.into()),
        }
    }

    pub fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        registry.register(self.socket()?, token, interest)
    }

    pub fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        registry.deregister(self.socket()?)
    }

    pub fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        registry.reregister(self.socket()?, token, interest)
    }

    pub fn wants_read(&self) -> bool {
        matches!(&self.state, State::Tls(s) if s.0.wants_read())
    }

    pub fn wants_write(&self) -> bool {
        matches!(&self.state, State::Tls(s) if s.0.wants_write())
    }
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.state {
            State::Blocking(s) => s.read(buf),
            State::Plain(s) => s.read(buf),
            State::Tls(s) => rustls::Stream::new(&mut s.0, &mut s.1).read(buf),
            State::Empty => Err(io::ErrorKind::NotConnected.into()),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.state {
            State::Blocking(s) => s.write(buf),
            State::Plain(s) => s.write(buf),
            State::Tls(s) => rustls::Stream::new(&mut s.0, &mut s.1).write(buf),
            State::Empty => Err(io::ErrorKind::NotConnected.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.state {
            State::Blocking(s) => s.flush(),
            State::Plain(s) => s.flush(),
            State::Tls(s) => rustls::Stream::new(&mut s.0, &mut s.1).flush(),
            State::Empty => Err(io::ErrorKind::NotConnected.into()),
        }
    }
}

use crate::protocol::{Pairing, SERVER_NAME};
use anyhow::Result;
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, ServerName},
};
use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub const IO_TIMEOUT: Duration = Duration::from_secs(3);
pub type ClientStream = StreamOwned<ClientConnection, DeadlineSocket>;
pub type ServerStream = StreamOwned<ServerConnection, DeadlineSocket>;

// Absolute deadlines prevent a peer from extending a packet forever by trickling bytes.
pub struct DeadlineSocket {
    socket: TcpStream,
    deadline: Instant,
}

impl DeadlineSocket {
    pub fn new(socket: TcpStream) -> Result<Self> {
        socket.set_nonblocking(false)?;
        socket.set_nodelay(true)?;
        Ok(Self {
            socket,
            deadline: Instant::now() + IO_TIMEOUT,
        })
    }
    pub fn renew(&mut self) {
        self.deadline = Instant::now() + IO_TIMEOUT;
    }
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Packet deadline expired"))
    }
}

impl Read for DeadlineSocket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(bytes)
    }
}

impl Write for DeadlineSocket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

#[derive(Default)]
pub struct Lifetime {
    stopped: AtomicBool,
    sockets: Mutex<Vec<TcpStream>>,
}

impl Lifetime {
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
    pub fn track(&self, socket: &TcpStream) -> Result<()> {
        let mut sockets = self.sockets.lock().unwrap();
        anyhow::ensure!(!self.stopped(), "Session stopped");
        sockets.push(socket.try_clone()?);
        Ok(())
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        for socket in self.sockets.lock().unwrap().drain(..) {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

pub fn client_config(pair: &Pairing) -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(pair.certificate.clone()))?;
    Ok(Arc::new(
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

pub fn connect(pair: &Pairing, config: Arc<ClientConfig>, life: &Lifetime) -> Result<ClientStream> {
    let socket = TcpStream::connect_timeout(&pair.address, IO_TIMEOUT)?;
    life.track(&socket)?;
    let mut stream = StreamOwned::new(
        ClientConnection::new(config, ServerName::try_from(SERVER_NAME)?)?,
        DeadlineSocket::new(socket)?,
    );
    stream.conn.complete_io(&mut stream.sock)?;
    Ok(stream)
}

pub fn accept(socket: TcpStream, config: Arc<ServerConfig>) -> Result<ServerStream> {
    let mut stream = StreamOwned::new(ServerConnection::new(config)?, DeadlineSocket::new(socket)?);
    stream.conn.complete_io(&mut stream.sock)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absolute_deadline_expires() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut timed = DeadlineSocket::new(socket).unwrap();
        timed.deadline = Instant::now() - Duration::from_millis(1);
        assert_eq!(
            timed.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn tls_verifies_paired_certificate_and_rejects_another_host() {
        use crate::protocol::{VERSION, read_json, write_json};
        use rustls::pki_types::PrivatePkcs8KeyDer;
        for trust_correct_cert in [true, false] {
            let identity = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()]).unwrap();
            let other = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()]).unwrap();
            let config = Arc::new(
                ServerConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![identity.cert.der().clone()],
                    PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
                )
                .unwrap(),
            );
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let pair = Pairing {
                version: VERSION,
                address: listener.local_addr().unwrap(),
                token: "a".repeat(64),
                certificate: if trust_correct_cert {
                    identity.cert.der().to_vec()
                } else {
                    other.cert.der().to_vec()
                },
            };
            let server = std::thread::spawn(move || -> Result<()> {
                let socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(e) => return Err(e.into()),
                    }
                };
                let mut stream = accept(socket, config)?;
                let text: String = read_json(&mut stream)?;
                write_json(&mut stream, &text)
            });
            let result = connect(&pair, client_config(&pair).unwrap(), &Lifetime::default());
            if trust_correct_cert {
                let mut stream = result.unwrap();
                write_json(&mut stream, &"encrypted loopback").unwrap();
                assert_eq!(
                    read_json::<String>(&mut stream).unwrap(),
                    "encrypted loopback"
                );
                assert!(server.join().unwrap().is_ok());
            } else {
                assert!(result.is_err());
                assert!(server.join().unwrap().is_err());
            }
        }
    }
}

use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    io,
    net::{SocketAddr, UdpSocket},
    sync::{Arc, LazyLock, Mutex, RwLock},
};

use socket2::SockRef;

use crate::error::Result;

// Apple sends back to the same UDP port number on the viewer. Distinct Macs
// using 5900/5901 therefore share each local socket, with separate SRTP keys.
static PORTS: LazyLock<RwLock<HashMap<SocketAddr, Port>>> = LazyLock::new(RwLock::default);
const BUFFER_BYTES: usize = 4 * 1024 * 1024;
const MAX_PACKETS: usize = 4096;

#[derive(Default)]
struct Queue {
    packets: VecDeque<Box<[u8]>>,
    bytes: usize,
}

impl Queue {
    fn push(&mut self, data: &[u8]) {
        while self.bytes + data.len() > BUFFER_BYTES || self.packets.len() >= MAX_PACKETS {
            let oldest = self.packets.pop_front().expect("queue exceeds its limit");
            self.bytes -= oldest.len();
        }
        self.packets.push_back(data.into());
        self.bytes += data.len();
    }

    fn pop(&mut self) -> Option<Box<[u8]>> {
        let data = self.packets.pop_front()?;
        self.bytes -= data.len();
        Some(data)
    }
}

struct Port {
    socket: UdpSocket,
    peers: Mutex<HashMap<SocketAddr, Queue>>,
}

pub(super) struct PeerSocket {
    local: SocketAddr,
    remote: SocketAddr,
}

impl PeerSocket {
    pub fn bind(mut local: SocketAddr, remote: SocketAddr) -> Result<Arc<Self>> {
        local.set_port(remote.port());
        let mut ports = PORTS
            .write()
            .map_err(|_| super::failed("UDP ports unavailable"))?;
        let port = match ports.entry(local) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let socket = UdpSocket::bind(local).map_err(|error| {
                    super::failed(format!("cannot bind UDP port {}: {error}", local.port()))
                })?;
                SockRef::from(&socket).set_recv_buffer_size(BUFFER_BYTES)?;
                socket.set_nonblocking(true)?;
                entry.insert(Port {
                    socket,
                    peers: Mutex::default(),
                })
            }
        };
        let peers = port
            .peers
            .get_mut()
            .map_err(|_| super::failed("UDP peers unavailable"))?;
        match peers.entry(remote) {
            Entry::Vacant(entry) => {
                entry.insert(Queue::default());
            }
            Entry::Occupied(_) => {
                return Err(super::failed(format!(
                    "another HEVC session is already using the Mac at {remote}"
                )));
            }
        }
        Ok(Arc::new(Self { local, remote }))
    }

    fn with_port<T>(&self, use_port: impl FnOnce(&Port) -> io::Result<T>) -> io::Result<T> {
        let ports = PORTS
            .read()
            .map_err(|_| io::Error::other("UDP ports unavailable"))?;
        // The last handle removes the port under the same lock in Drop.
        use_port(&ports[&self.local])
    }

    pub fn send(&self, data: &[u8]) -> io::Result<usize> {
        self.with_port(|port| port.socket.send_to(data, self.remote))
    }

    pub fn recv(&self, buffer: &mut [u8; 65_536]) -> io::Result<usize> {
        self.with_port(|port| {
            let mut peers = port
                .peers
                .lock()
                .map_err(|_| io::Error::other("UDP peers unavailable"))?;
            if let Some(data) = peers.get_mut(&self.remote).and_then(Queue::pop) {
                buffer[..data.len()].copy_from_slice(&data);
                return Ok(data.len());
            }
            // Deliver our own packet in place. Queue interleaved packets for
            // registered peers before their reader next polls this port.
            for _ in 0..64 {
                let (len, sender) = port.socket.recv_from(buffer)?;
                if sender == self.remote {
                    return Ok(len);
                }
                if let Some(queue) = peers.get_mut(&sender) {
                    queue.push(&buffer[..len]);
                }
            }
            Err(io::ErrorKind::WouldBlock.into())
        })
    }
}

impl Drop for PeerSocket {
    fn drop(&mut self) {
        let mut ports = PORTS.write().unwrap_or_else(|error| error.into_inner());
        if let Entry::Occupied(mut entry) = ports.entry(self.local) {
            let peers = entry
                .get_mut()
                .peers
                .get_mut()
                .unwrap_or_else(|error| error.into_inner());
            peers.remove(&self.remote);
            if peers.is_empty() {
                entry.remove();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn same_port_routes_only_registered_peers_and_releases_the_last_socket() {
        let first = UdpSocket::bind(("127.0.0.2", 0)).unwrap();
        let first_address = first.local_addr().unwrap();
        let second = UdpSocket::bind(("127.0.0.3", first_address.port())).unwrap();
        let stranger = UdpSocket::bind(("127.0.0.4", first_address.port())).unwrap();
        let local = SocketAddr::from(([127, 0, 0, 1], first_address.port()));
        let a = PeerSocket::bind(local, first_address).unwrap();
        let b = PeerSocket::bind(local, second.local_addr().unwrap()).unwrap();
        assert!(PeerSocket::bind(local, first_address).is_err());
        let clone = a.clone();
        drop(a);
        stranger.send_to(b"unknown", local).unwrap();
        second.send_to(b"second", local).unwrap();
        first.send_to(b"first", local).unwrap();
        let mut data = [0; 65_536];
        let read = |socket: &PeerSocket, buffer: &mut [u8; 65_536]| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match socket.recv(buffer) {
                    Ok(len) => break len,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::yield_now()
                    }
                    Err(error) => panic!("reading routed datagram: {error}"),
                }
            }
        };
        let n = read(&clone, &mut data);
        assert_eq!(&data[..n], b"first");
        drop(clone);
        let n = read(&b, &mut data);
        assert_eq!(&data[..n], b"second");
        assert_eq!(
            b.recv(&mut data).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let reopened = PeerSocket::bind(local, first_address).unwrap();
        reopened.send(b"return").unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (n, sender) = first.recv_from(&mut data).unwrap();
        assert_eq!((sender, &data[..n]), (local, b"return".as_slice()));
        drop(reopened);
        drop(b);
        UdpSocket::bind(local).unwrap();
    }

    #[test]
    fn slow_peer_queue_keeps_recent_packets_within_both_limits() {
        let mut queue = Queue::default();
        for _ in 0..=MAX_PACKETS {
            queue.push(&[]);
        }
        assert_eq!(queue.packets.len(), MAX_PACKETS);
        let large = vec![1; 65_536];
        for _ in 0..=BUFFER_BYTES / large.len() {
            queue.push(&large);
        }
        queue.push(b"latest");
        assert!(queue.bytes <= BUFFER_BYTES);
        assert_eq!(queue.packets.back().unwrap().as_ref(), b"latest");
    }
}

use std::fmt::Debug;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use serde_big_array::BigArray;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;


mod interval;
use interval::Interval;
use tracing::trace;

mod notify;
use notify::Notify;

use crate::Id;
mod builder;
use builder::Port;
pub use builder::ChartBuilder;

mod iter;

use self::interval::Until;

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct DiscoveryMsg<const N: usize, T>
where
    T: Serialize + DeserializeOwned,
{
    header: u64,
    id: Id,
    #[serde(with = "BigArray")]
    msg: [T; N],
}

#[derive(Debug, Clone)]
pub struct Entry<Msg: Debug + Clone> {
    pub ip: IpAddr,
    pub msg: Msg,
}

#[derive(Debug, Clone)]
pub struct Chart<const N: usize, T: Debug + Clone + Serialize> {
    header: u64,
    service_id: Id,
    msg: [T; N],
    sock: Arc<UdpSocket>,
    interval: Interval,
    map: Arc<dashmap::DashMap<Id, Entry<[T; N]>>>,
    broadcast: broadcast::Sender<(Id, Entry<[T; N]>)>,
}

impl<const N: usize, T: Serialize + Debug + Clone> Chart<N, T> {
    fn insert(&self, id: Id, entry: Entry<[T; N]>) -> bool {
        let old_key = self.map.insert(id, entry.clone());
        if old_key.is_none() {
            // errors if there are no active recievers which is
            // the default and not a problem
            let _ig_err = self.broadcast.send((id, entry));
            true
        } else {
            false
        }
    }

    #[tracing::instrument(skip(self, buf))]
    fn process_buf<'de>(&self, buf: &'de [u8], addr: SocketAddr) -> bool
    where
        T: Serialize + DeserializeOwned + Debug,
    {
        let DiscoveryMsg::<N, T> { header, id, msg } = bincode::deserialize(buf).unwrap();
        if header != self.header {
            return false;
        }
        if id == self.service_id {
            return false;
        }
        self.insert(id, Entry { ip: addr.ip(), msg })
    }
}

impl<const N: usize> Chart<N, Port> {
    #[must_use]
    pub fn our_service_ports(&self) -> &[u16] {
        &self.msg
    }
}

impl<T: Debug + Clone + Serialize> Chart<1, T> {
    #[must_use]
    pub fn our_msg(&self) -> &T {
        &self.msg[0]
    }
}

impl Chart<1, Port> {
    #[must_use]
    pub fn our_service_port(&self) -> u16 {
        self.msg[0]
    }
}

impl<const N: usize, T: Debug + Clone + Serialize + DeserializeOwned> Chart<N, T> {
    pub async fn notify(&self) -> Notify<N, T> {
        Notify(self.broadcast.subscribe())
    }

    #[must_use]
    pub fn entries(&self) -> Vec<Entry<[T; N]>> {
        self.map.iter().map(|m| m.value().clone()).collect()
    }

    /// members discoverd including self
    #[must_use]
    pub fn size(&self) -> usize {
        self.map.len() + 1
    }

    #[must_use]
    pub fn our_id(&self) -> u64 {
        self.service_id
    }

    #[must_use]
    pub fn discovery_port(&self) -> u16 {
        self.sock.local_addr().unwrap().port()
    }

    #[must_use]
    fn discovery_msg(&self) -> DiscoveryMsg<N, T> {
        DiscoveryMsg {
            header: self.header,
            id: self.service_id,
            msg: self.msg.clone(),
        }
    }

    #[must_use]
    fn discovery_buf(&self) -> Vec<u8> {
        let msg = self.discovery_msg();
        bincode::serialize(&msg).unwrap()
    }

    #[must_use]
    fn broadcast_soon(&mut self) -> bool {
        let next = self.interval.next();
        next.until() < Duration::from_millis(100)
    }
}

#[tracing::instrument]
pub async fn handle_incoming<const N: usize, T>(mut chart: Chart<N, T>)
where
    T: Debug + Clone + Serialize + DeserializeOwned,
{
    loop {
        let mut buf = [0; 1024];
        let (_len, addr) = chart.sock.recv_from(&mut buf).await.unwrap();
        trace!("got msg from: {addr:?}");
        let was_uncharted = chart.process_buf(&buf, addr);
        if was_uncharted && !chart.broadcast_soon() {
            chart
                .sock
                .send_to(&chart.discovery_buf(), addr)
                .await
                .unwrap();
        }
    }
}

#[tracing::instrument]
pub async fn broadcast_periodically<const N: usize, T>(mut chart: Chart<N, T>, period: Duration)
where
    T: Debug + Serialize + DeserializeOwned + Clone,
{
    loop {
        chart.interval.sleep_till_next().await;
        trace!("sending discovery msg");
        broadcast(&chart.sock, &chart.discovery_buf()).await;
    }
}

#[tracing::instrument]
async fn broadcast(sock: &Arc<UdpSocket>, msg: &[u8]) {
    let multiaddr = Ipv4Addr::from([224, 0, 0, 251]);
    let _len = sock.send_to(msg, (multiaddr, 8080)).await.unwrap();
}

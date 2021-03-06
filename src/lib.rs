//! Redirect traffic to a SOCKS proxy with pcap.

use ipnetwork::Ipv4Network;
use log::{debug, info, trace, warn};
use lru::LruCache;
use rand::{self, Rng};
use std::cmp::{max, min};
use std::collections::{HashMap, VecDeque};
use std::fmt::{self, Display};
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::io;

pub mod cache;
pub mod packet;
pub mod pcap;
pub mod socks;

use self::socks::{
    DatagramWorker, ForwardDatagram, ForwardStream, SocksAuth, SocksOption, StreamWorker,
};
use cache::{Queue, Window};
use packet::layer::arp::Arp;
use packet::layer::ethernet::Ethernet;
use packet::layer::icmpv4::Icmpv4;
use packet::layer::ipv4::Ipv4;
use packet::layer::tcp::Tcp;
use packet::layer::udp::Udp;
use packet::layer::{Layer, LayerKind, LayerKinds, Layers};
use packet::{Defraggler, Indicator};
use pcap::Interface;
use pcap::{HardwareAddr, Receiver, Sender};

/// Gets a list of available network interfaces for the current machine.
pub fn interfaces() -> Vec<Interface> {
    pcap::interfaces()
        .into_iter()
        .filter(|inter| inter.is_up() && !inter.is_loopback())
        .collect()
}

/// Gets an available network interface.
pub fn interface(name: Option<String>) -> Option<Interface> {
    let mut inters = match name {
        Some(ref name) => {
            let mut inters = interfaces();
            inters.retain(|ref inter| inter.name() == name);

            inters
        }
        None => interfaces(),
    };

    if inters.len() != 1 {
        None
    } else {
        Some(inters.pop().unwrap())
    }
}

/// Represents a timer.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timer {
    instant: Instant,
    timeout: Duration,
}

impl Timer {
    /// Creates a new `Timer`.
    pub fn new(timeout: u64) -> Timer {
        Timer {
            instant: Instant::now(),
            timeout: Duration::from_millis(timeout),
        }
    }

    /// Returns the amount of time elapsed since this timer was created.
    pub fn elapsed(&self) -> Duration {
        self.instant.elapsed()
    }

    /// Returns if the timer is timed out.
    pub fn is_timedout(&self) -> bool {
        self.instant.elapsed() > self.timeout
    }
}

/// Represents the max distance of `u32` values between packets in an `u32` window.
const MAX_U32_WINDOW_SIZE: usize = 16 * 1024 * 1024;

/// Represents the receive window size.
const RECV_WINDOW: u16 = u16::MAX;

/// Represents if the RTO computation is enabled.
const ENABLE_RTO_COMPUTE: bool = true;
/// Represents the initial timeout for a retransmission in a TCP connection.
const INITIAL_RTO: u64 = 1000;
/// Represents the minimum timeout for a retransmission in a TCP connection.
const MIN_RTO: u64 = 1000;
/// Represents the maximum timeout for a retransmission in a TCP connection.
const MAX_RTO: u64 = 60000;

/// Represents the TX state of a TCP connection.
pub struct TcpTxState {
    src: SocketAddrV4,
    dst: SocketAddrV4,
    send_window: usize,
    send_wscale: Option<u8>,
    sack_perm: bool,
    sequence: u32,
    acknowledgement: u32,
    window: u16,
    sacks: Option<Vec<(u32, u32)>>,
    cache: Queue,
    cache_syn: Option<Instant>,
    cache_fin: Option<Timer>,
    cache_fin_retrans: bool,
    queue: VecDeque<u8>,
    queue_fin: bool,
    rto: u64,
    srtt: Option<u64>,
    rttvar: Option<u64>,
}

impl TcpTxState {
    /// Creates a new `TcpTxState`.
    pub fn new(
        src: SocketAddrV4,
        dst: SocketAddrV4,
        sequence: u32,
        acknowledgement: u32,
        send_window: u16,
        send_wscale: Option<u8>,
        sack_perm: bool,
        wscale: Option<u8>,
    ) -> TcpTxState {
        TcpTxState {
            src,
            dst,
            send_window: (send_window as usize) << send_wscale.unwrap_or(0),
            send_wscale,
            sack_perm,
            sequence,
            acknowledgement,
            window: RECV_WINDOW,
            sacks: None,
            cache: Queue::with_capacity(
                (RECV_WINDOW as usize) << wscale.unwrap_or(0) as usize,
                sequence,
            ),
            cache_syn: None,
            cache_fin: None,
            cache_fin_retrans: true,
            queue: VecDeque::new(),
            queue_fin: false,
            rto: INITIAL_RTO,
            srtt: None,
            rttvar: None,
        }
    }

    /// Sets the window of the TCP connection.
    pub fn set_send_window(&mut self, window: usize) {
        self.send_window = window;
        trace!(
            "set TCP send window of {} -> {} to {}",
            self.dst,
            self.src,
            window
        );
    }

    /// Adds sequence to the TCP connection.
    pub fn add_sequence(&mut self, n: u32) {
        self.sequence = self
            .sequence
            .checked_add(n)
            .unwrap_or_else(|| n - (u32::MAX - self.sequence));
        trace!(
            "add TCP sequence of {} -> {} to {}",
            self.dst,
            self.src,
            self.sequence
        );
    }

    /// Adds acknowledgement to the TCP connection.
    pub fn add_acknowledgement(&mut self, n: u32) {
        self.acknowledgement = self
            .acknowledgement
            .checked_add(n)
            .unwrap_or_else(|| n - (u32::MAX - self.acknowledgement));
        trace!(
            "add TCP acknowledgement of {} -> {} to {}",
            self.dst,
            self.src,
            self.acknowledgement
        );
    }

    /// Sets the window of the TCP connection.
    pub fn set_window(&mut self, window: u16) {
        self.window = window;
        trace!(
            "set TCP window of {} -> {} to {}",
            self.dst,
            self.src,
            window
        );
    }

    /// Sets the SACKs of the TCP connection.
    pub fn set_sacks(&mut self, sacks: &Vec<(u32, u32)>) {
        if sacks.is_empty() {
            self.sacks = None;
            trace!("remove TCP SACK of {} -> {}", self.dst, self.src);
        } else {
            let size = min(4, sacks.len());
            self.sacks = Some(Vec::from(&sacks[..size]));

            let mut desc = format!("[{}, {}]", sacks[0].0, sacks[0].1);
            if sacks.len() > 1 {
                desc.push_str(format!(" and {} more", sacks.len() - 1).as_str());
            }
            trace!("set TCP SACK of {} -> {} to {}", self.dst, self.src, desc);
        }
    }

    /// Acknowledges to the given sequence of the TCP connection.
    pub fn acknowledge(&mut self, sequence: u32) {
        let mut rtt = None;

        // SYN
        if let Some(instant) = self.cache_syn {
            let send_next = self.sequence;
            if sequence
                .checked_sub(send_next)
                .unwrap_or_else(|| sequence + (u32::MAX - send_next)) as usize
                <= MAX_U32_WINDOW_SIZE
            {
                rtt = Some(instant.elapsed());

                self.cache_syn = None;
                trace!("acknowledge TCP SYN of {} -> {}", self.dst, self.src);

                // Update TCP sequence
                self.add_sequence(1);
            }
        }

        // Invalidate cache
        let cache_rtt = self.cache.invalidate_to(sequence);
        if rtt.is_none() {
            rtt = cache_rtt;
        }
        trace!(
            "acknowledge TCP cache of {} -> {} to sequence {}",
            self.dst,
            self.src,
            sequence
        );

        if sequence
            .checked_sub(self.cache.recv_next())
            .unwrap_or_else(|| sequence + (u32::MAX - self.cache.recv_next())) as usize
            <= MAX_U32_WINDOW_SIZE
        {
            if let Some(timer) = self.cache_fin {
                if rtt.is_none() && !self.cache_fin_retrans && !timer.is_timedout() {
                    rtt = Some(timer.elapsed());
                }

                self.cache_fin = None;
                self.cache_fin_retrans = false;
                trace!("acknowledge TCP FIN of {} -> {}", self.dst, self.src);

                // Update TCP sequence
                self.add_sequence(1);
            }
        }

        // Update RTO
        if let Some(rtt) = rtt {
            self.update_rto(rtt);
        }
    }

    /// Updates the TCP SYN timer of the TCP connection.
    pub fn update_syn_timer(&mut self) {
        self.cache_syn = Some(Instant::now());
        trace!("update TCP SYN timer of {} -> {}", self.dst, self.src);
    }

    /// Updates the TCP FIN timer of the TCP connection.
    pub fn update_fin_timer(&mut self) {
        if self.cache_fin.is_some() {
            self.cache_fin_retrans = true;
        }
        self.cache_fin = Some(Timer::new(self.rto));
        trace!("update TCP FIN timer of {} -> {}", self.dst, self.src);
    }

    /// Appends the payload from the queue to the cache of the TCP connection.
    pub fn append_cache(&mut self, size: usize) -> io::Result<Vec<u8>> {
        let payload = self.queue.drain(..size).collect::<Vec<_>>();

        // Append to cache
        trace!(
            "append {} Bytes to TCP cache of {} -> {}",
            payload.len(),
            self.dst,
            self.src
        );
        self.cache.append(&payload, self.rto)?;

        Ok(payload)
    }

    /// Appends the TCP FIN from the queue to the cache of the TCP connection.
    pub fn append_cache_fin(&mut self) {
        self.queue_fin = false;
        trace!(
            "append TCP FIN to TCP cache of {} -> {}",
            self.dst,
            self.src
        );
        self.update_fin_timer();
    }

    /// Appends the payload to the queue of the TCP connection.
    pub fn append_queue(&mut self, payload: &[u8]) {
        self.queue.extend(payload);
        trace!(
            "append {} Bytes to TCP queue of {} -> {}",
            payload.len(),
            self.dst,
            self.src
        );
    }

    /// Appends the TCP FIN to the queue of the TCP connection.
    pub fn append_queue_fin(&mut self) {
        self.queue_fin = true;
        trace!(
            "append TCP FIN to TCP queue of {} -> {}",
            self.dst,
            self.src
        );
    }

    fn set_rto(&mut self, rto: u64) {
        if ENABLE_RTO_COMPUTE {
            let rto = min(MAX_RTO, max(MIN_RTO, rto));

            self.rto = rto;
            trace!("set TCP RTO of {} -> {} to {}", self.dst, self.src, rto);
        }
    }

    /// Doubles the RTO of the TCP connection.
    pub fn double_rto(&mut self) {
        self.set_rto(self.rto.checked_mul(2).unwrap_or(u64::MAX));
    }

    /// Updates the RTO of the TCP connection.
    pub fn update_rto(&mut self, rtt: Duration) {
        let rtt = if rtt.as_millis() > u64::MAX as u128 {
            u64::MAX
        } else {
            rtt.as_millis() as u64
        };

        let srtt;
        let rttvar;
        match self.srtt {
            Some(prev_srtt) => {
                // RTTVAR
                let prev_rttvar = self.rttvar.unwrap();
                rttvar = (prev_rttvar / 8 * 7)
                    .checked_add(
                        prev_srtt
                            .checked_sub(rtt)
                            .unwrap_or_else(|| rtt - prev_srtt)
                            / 4,
                    )
                    .unwrap_or(u64::MAX);

                // SRTT
                srtt = (prev_rttvar / 8 * 7)
                    .checked_add(rtt / 8)
                    .unwrap_or(u64::MAX);
            }
            None => {
                // SRTT
                srtt = rtt;

                // RTTVAR
                rttvar = rtt / 2;
            }
        }

        // SRTT
        self.srtt = Some(srtt);
        trace!("set TCP SRTT of {} -> {} to {}", self.dst, self.src, srtt);

        // RTTVAR
        self.rttvar = Some(rttvar);
        trace!(
            "set TCP RTTVAR of {} -> {} to {}",
            self.dst,
            self.src,
            rttvar
        );

        // RTO
        let rto = srtt
            .checked_add(max(1, rttvar.checked_mul(4).unwrap_or(u64::MAX)))
            .unwrap_or(u64::MAX);
        self.set_rto(rto);
    }

    /// Returns the send window of the TCP connection. The send window represents the received
    /// window from the source and indicates how much payload it can receive next.
    pub fn send_window(&self) -> usize {
        self.send_window
    }

    /// Returns the send window scale of the TCP connection.
    pub fn send_wscale(&self) -> Option<u8> {
        self.send_wscale
    }

    /// Returns if the SACK is permitted of the TCP connection.
    pub fn sack_perm(&self) -> bool {
        self.sack_perm
    }

    /// Returns the sequence of the TCP connection.
    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    /// Returns the acknowledgement of the TCP connection.
    pub fn acknowledgement(&self) -> u32 {
        self.acknowledgement
    }

    /// Returns the window of the TCP connection.
    pub fn window(&self) -> u16 {
        self.window
    }

    /// Returns the SACKs of the TCP connection.
    pub fn sacks(&self) -> &Option<Vec<(u32, u32)>> {
        &self.sacks
    }

    /// Returns the cache of the TCP connection.
    pub fn cache(&self) -> &Queue {
        &self.cache
    }

    /// Returns the mutable cache of the TCP connection.
    pub fn cache_mut(&mut self) -> &mut Queue {
        &mut self.cache
    }

    /// Returns the TCP SYN in the cache of the TCP connection.
    pub fn cache_syn(&self) -> Option<Instant> {
        self.cache_syn
    }

    /// Returns the TCP FIN in the cache of the TCP connection.
    pub fn cache_fin(&self) -> Option<Timer> {
        self.cache_fin
    }

    /// Returns the queue of the TCP connection.
    pub fn queue(&self) -> &VecDeque<u8> {
        &self.queue
    }

    /// Returns if the TCP FIN is in the queue of the TCP connection.
    pub fn queue_fin(&self) -> bool {
        self.queue_fin
    }

    /// Returns the RTO if the TCP connection.
    pub fn rto(&self) -> u64 {
        self.rto
    }
}

impl Display for TcpTxState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TCP TX State: {} -> {}", self.dst, self.src)
    }
}

/// Represents the wait time after a `TimedOut` `IoError`.
const TIMEDOUT_WAIT: u64 = 20;

/// Represents if the receive-side silly window syndrome avoidance is enabled.
const ENABLE_RECV_SWS_AVOID: bool = true;
/// Represents if the send-side silly window syndrome avoidance is enabled.
const ENABLE_SEND_SWS_AVOID: bool = true;

/// Represents if the TCP MSS option is enabled.
const ENABLE_MSS: bool = true;

/// Represents the minimum frame size.
/// Because all traffic is in Ethernet, and the 802.3 specifies the minimum is 64 Bytes.
/// Exclude the 4 bytes used in FCS, the minimum frame size in pcap2socks is 60 Bytes.
const MINIMUM_FRAME_SIZE: usize = 60;

/// Represents a channel forward traffic to the source in pcap.
pub struct Forwarder {
    tx: Sender,
    src_mtu: HashMap<Ipv4Addr, usize>,
    local_mtu: usize,
    src_hardware_addr: HashMap<Ipv4Addr, HardwareAddr>,
    local_hardware_addr: HardwareAddr,
    local_ip_addr: Ipv4Addr,
    ipv4_identification_map: HashMap<(Ipv4Addr, Ipv4Addr), u16>,
    states: HashMap<(SocketAddrV4, SocketAddrV4), TcpTxState>,
}

impl Forwarder {
    /// Creates a new `Forwarder`.
    pub fn new(
        tx: Sender,
        mtu: usize,
        local_hardware_addr: HardwareAddr,
        local_ip_addr: Ipv4Addr,
    ) -> Forwarder {
        Forwarder {
            tx,
            src_mtu: HashMap::new(),
            local_mtu: mtu,
            src_hardware_addr: HashMap::new(),
            local_hardware_addr,
            local_ip_addr,
            ipv4_identification_map: HashMap::new(),
            states: HashMap::new(),
        }
    }

    /// Sets the source MTU.
    pub fn set_src_mtu(&mut self, src_ip_addr: Ipv4Addr, mtu: usize) -> bool {
        let prev_mtu = *self.src_mtu.get(&src_ip_addr).unwrap_or(&self.local_mtu);

        self.src_mtu.insert(src_ip_addr, min(self.local_mtu, mtu));
        trace!("set source MTU of {} to {}", src_ip_addr, mtu);

        return *self.src_mtu.get(&src_ip_addr).unwrap_or(&self.local_mtu) != prev_mtu;
    }

    /// Sets the source hardware address.
    pub fn set_src_hardware_addr(&mut self, src_ip_addr: Ipv4Addr, hardware_addr: HardwareAddr) {
        self.src_hardware_addr.insert(src_ip_addr, hardware_addr);
        trace!(
            "set source hardware address of {} to {}",
            src_ip_addr,
            hardware_addr
        );
    }

    /// Sets the local IP address.
    pub fn set_local_ip_addr(&mut self, ip_addr: Ipv4Addr) {
        self.local_ip_addr = ip_addr;
        trace!("set local IP address to {}", ip_addr);
    }

    fn increase_ipv4_identification(&mut self, dst_ip_addr: Ipv4Addr, src_ip_addr: Ipv4Addr) {
        let entry = self
            .ipv4_identification_map
            .entry((src_ip_addr, dst_ip_addr))
            .or_insert(0);
        *entry = entry.checked_add(1).unwrap_or(0);
        trace!(
            "increase IPv4 identification of {} -> {} to {}",
            dst_ip_addr,
            src_ip_addr,
            entry
        );
    }

    /// Sets the state of a TCP connection.
    pub fn set_state(&mut self, dst: SocketAddrV4, src: SocketAddrV4, state: TcpTxState) {
        let key = (src, dst);

        self.states.insert(key, state);
    }

    /// Returns the state of a TCP connection.
    pub fn get_state(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> Option<&mut TcpTxState> {
        let key = (src, dst);

        self.states.get_mut(&key)
    }

    fn get_tcp_window(&self, dst: SocketAddrV4, src: SocketAddrV4) -> u16 {
        let key = (src, dst);

        let state = self.states.get(&key).unwrap();

        // Avoid SWS
        if ENABLE_RECV_SWS_AVOID {
            let thresh = min((RECV_WINDOW / 2) as usize, self.local_mtu);

            if (state.window() as usize) < thresh {
                0
            } else {
                state.window()
            }
        } else {
            state.window()
        }
    }

    /// Removes all information related to a TCP connection.
    pub fn clean_up(&mut self, dst: SocketAddrV4, src: SocketAddrV4) {
        let key = (src, dst);

        self.states.remove(&key);
    }

    /// Returns the size of the cache and the queue of a TCP connection.
    pub fn get_cache_size(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> usize {
        let key = (src, dst);

        let state = self.states.get(&key).unwrap();

        state.cache().len() + state.queue().len()
    }

    /// Sends an ARP reply packet.
    pub fn send_arp_reply(&mut self, src_ip_addr: Ipv4Addr) -> io::Result<()> {
        // ARP
        let arp = Arp::new_reply(
            self.local_hardware_addr,
            self.local_ip_addr,
            *self
                .src_hardware_addr
                .get(&src_ip_addr)
                .unwrap_or(&pcap::HARDWARE_ADDR_UNSPECIFIED),
            src_ip_addr,
        );

        // Ethernet
        let ethernet =
            Ethernet::new(arp.kind(), arp.src_hardware_addr(), arp.dst_hardware_addr()).unwrap();

        // Indicator
        let indicator = Indicator::new(Layers::Ethernet(ethernet), Some(Layers::Arp(arp)), None);

        // Send
        self.send(&indicator)
    }

    /// Appends TCP ACK payload to the queue.
    pub fn append_to_queue(
        &mut self,
        dst: SocketAddrV4,
        src: SocketAddrV4,
        payload: &[u8],
    ) -> io::Result<()> {
        // Append to queue
        let state = self.get_state(dst, src).unwrap();
        state.append_queue(payload);

        self.send_tcp_ack(dst, src)
    }

    /// Retransmits TCP ACK packets from the cache. This method is used for fast retransmission.
    pub fn retransmit_tcp_ack(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        let key = (src, dst);

        // Retransmit
        let state = self.states.get(&key).unwrap();
        let payload = state.cache().get_all();
        let sequence = state.cache().sequence();
        let size = state.cache().len();

        if payload.len() > 0 {
            if size == payload.len() && state.cache_fin().is_some() {
                // ACK/FIN
                trace!(
                    "retransmit TCP ACK/FIN ({} Bytes) {} -> {} from {}",
                    payload.len(),
                    dst,
                    src,
                    sequence
                );

                // Send
                self.send_tcp_ack_raw(dst, src, sequence, payload.as_slice(), true)?;
            } else {
                // ACK
                trace!(
                    "retransmit TCP ACK ({} Bytes) {} -> {} from {}",
                    payload.len(),
                    dst,
                    src,
                    sequence
                );

                // Send
                self.send_tcp_ack_raw(dst, src, sequence, payload.as_slice(), false)?;
            }
        }

        Ok(())
    }

    /// Retransmits TCP ACK packets from the cache excluding the certain edges. This method is used
    /// for fast retransmission.
    pub fn retransmit_tcp_ack_without(
        &mut self,
        dst: SocketAddrV4,
        src: SocketAddrV4,
        sacks: Vec<(u32, u32)>,
    ) -> io::Result<()> {
        let key = (src, dst);

        let state = self.states.get(&key).unwrap();
        let sequence = state.cache().sequence();
        let recv_next = state.cache().recv_next();

        // Find all disjointed ranges
        let mut ranges = Vec::new();
        ranges.push((sequence, recv_next));
        for sack in sacks {
            let mut temp_ranges = Vec::new();

            for range in ranges {
                for temp_range in disjoint_u32_range(range, sack) {
                    temp_ranges.push(temp_range);
                }
            }

            ranges = temp_ranges;
        }
        let ranges = ranges;

        // Retransmit
        for range in &ranges {
            let size = range
                .1
                .checked_sub(range.0)
                .unwrap_or_else(|| range.1 + (u32::MAX - range.0)) as usize;
            let state = self.states.get(&key).unwrap();
            let payload = state.cache().get(range.0, size)?;
            if payload.len() > 0 {
                if range.1 == recv_next && state.cache_fin().is_some() {
                    // ACK/FIN
                    trace!(
                        "retransmit TCP ACK/FIN ({} Bytes) {} -> {} from {}",
                        payload.len(),
                        dst,
                        src,
                        sequence
                    );

                    // Send
                    self.send_tcp_ack_raw(dst, src, range.0, payload.as_slice(), true)?;
                } else {
                    // ACK
                    trace!(
                        "retransmit TCP ACK ({} Bytes) {} -> {} from {}",
                        payload.len(),
                        dst,
                        src,
                        sequence
                    );

                    // Send
                    self.send_tcp_ack_raw(dst, src, range.0, payload.as_slice(), false)?;
                }
            }
        }

        // Pure FIN
        let state = self.states.get(&key).unwrap();
        if ranges.len() == 0 && state.cache_fin().is_some() {
            // FIN
            trace!("retransmit TCP FIN {} -> {}", dst, src);

            // Send
            self.send_tcp_fin(dst, src)?;
        }

        Ok(())
    }

    /// Retransmits timed out TCP ACK packets from the cache. This method is used for transmitting
    /// timed out data.
    pub fn retransmit_tcp_ack_timedout(
        &mut self,
        dst: SocketAddrV4,
        src: SocketAddrV4,
    ) -> io::Result<()> {
        let state = self.get_state(dst, src).unwrap();
        let next_rto = state.rto().checked_mul(2).unwrap_or(u64::MAX);
        let payload = state
            .cache_mut()
            .get_timed_out_and_update(max(MAX_RTO, min(MIN_RTO, next_rto)));
        let sequence = state.cache().sequence();
        let size = state.cache().len();

        if size > 0 {
            // Double RTO
            state.double_rto();

            // If all the cache is get, the FIN should also be sent
            if size == payload.len() && state.cache_fin().is_some() {
                // ACK/FIN
                state.update_fin_timer();
                trace!(
                    "retransmit TCP ACK/FIN ({} Bytes) and FIN {} -> {} from {} due to timeout",
                    payload.len(),
                    dst,
                    src,
                    sequence
                );

                // Send
                self.send_tcp_ack_raw(dst, src, sequence, payload.as_slice(), true)?;
            } else {
                // ACK
                trace!(
                    "retransmit TCP ACK ({} Bytes) {} -> {} from {} due to timeout",
                    payload.len(),
                    dst,
                    src,
                    sequence
                );

                // Send
                self.send_tcp_ack_raw(dst, src, sequence, payload.as_slice(), false)?;
            }
        } else {
            // FIN
            if let Some(timer) = state.cache_fin() {
                if timer.is_timedout() {
                    // Double RTO
                    state.double_rto();
                    state.update_fin_timer();
                    trace!("retransmit TCP FIN {} -> {} due to timeout", dst, src);

                    // Send
                    self.send_tcp_fin(dst, src)?;
                }
            }
        }

        Ok(())
    }

    /// Sends TCP ACK packets from the queue.
    pub fn send_tcp_ack(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        let key = (src, dst);

        // Retransmit unhandled SYN
        let state = self.states.get(&key).unwrap();
        if state.cache_syn().is_some() {
            return self.send_tcp_ack_syn(dst, src);
        }

        if state.send_window() > 0 {
            // TCP sequence
            let sent_size = state.cache().len();
            let remain_size = state.send_window().checked_sub(sent_size).unwrap_or(0);
            let remain_size = min(remain_size, u16::MAX as usize) as u16;

            let mut size = min(remain_size as usize, state.queue().len());
            // Avoid SWS
            if ENABLE_SEND_SWS_AVOID {
                let mtu = *self.src_mtu.get(src.ip()).unwrap_or(&self.local_mtu);
                let mss = mtu - (Ipv4::minimum_len() + Tcp::minimum_len());

                if size < mss && !state.cache().is_empty() {
                    size = 0;
                }
            }
            let size = size;
            if size > 0 {
                let state = self.get_state(dst, src).unwrap();
                let payload = state.append_cache(size)?;

                // If the queue is empty and a FIN is in the queue, pop it
                if state.queue().is_empty() && state.queue_fin() {
                    // ACK/FIN
                    state.append_cache_fin();

                    // Send
                    let state = self.states.get(&key).unwrap();
                    let sequence = state.sequence();
                    self.send_tcp_ack_raw(dst, src, sequence, &payload, true)?;
                } else {
                    // ACK
                    let state = self.states.get(&key).unwrap();
                    let sequence = state.sequence();
                    self.send_tcp_ack_raw(dst, src, sequence, &payload, false)?;
                }
            }
        }

        // If the queue is empty and a FIN is in the queue, pop it
        // FIN
        let state = self.get_state(dst, src).unwrap();
        if state.queue_fin() {
            if state.cache().is_empty() {
                // FIN
                state.append_cache_fin();

                // Send
                self.send_tcp_fin(dst, src)?;
            }
        }

        Ok(())
    }

    fn send_tcp_ack_raw(
        &mut self,
        dst: SocketAddrV4,
        src: SocketAddrV4,
        sequence: u32,
        payload: &[u8],
        is_fin: bool,
    ) -> io::Result<()> {
        let key = (src, dst);

        // Segmentation
        let mss = *self.src_mtu.get(src.ip()).unwrap_or(&self.local_mtu)
            - (Ipv4::minimum_len() + Tcp::minimum_len());
        let mut i = 0;
        while mss * i < payload.len() {
            let state = self.states.get(&key).unwrap();
            let size = min(mss, payload.len() - i * mss);
            let payload = &payload[i * mss..i * mss + size];
            let sequence = sequence
                .checked_add((i * mss) as u32)
                .unwrap_or_else(|| (i * mss) as u32 - (u32::MAX - sequence));
            let mut recv_next = sequence
                .checked_add(size as u32)
                .unwrap_or_else(|| size as u32 - (u32::MAX - sequence));

            // TCP
            let tcp;
            if is_fin && mss * (i + 1) >= payload.len() {
                // ACK/FIN
                tcp = Tcp::new_ack_fin(
                    dst.port(),
                    src.port(),
                    sequence,
                    state.acknowledgement(),
                    self.get_tcp_window(dst, src),
                    None,
                );
                recv_next = recv_next.checked_add(1).unwrap_or(0);
            } else {
                // ACK
                tcp = Tcp::new_ack(
                    dst.port(),
                    src.port(),
                    sequence,
                    state.acknowledgement(),
                    self.get_tcp_window(dst, src),
                    None,
                    None,
                );
            }

            // Send
            self.send_ipv4_with_transport(
                dst.ip().clone(),
                src.ip().clone(),
                Layers::Tcp(tcp),
                Some(payload),
            )?;

            // Update TCP sequence
            let state = self.get_state(dst, src).unwrap();
            let record_sequence = state.sequence();
            let sub_sequence = recv_next
                .checked_sub(record_sequence)
                .unwrap_or_else(|| recv_next + (u32::MAX - record_sequence));
            if (sub_sequence as usize) <= MAX_U32_WINDOW_SIZE {
                state.add_sequence(sub_sequence);
            }

            i = i + 1;
        }

        Ok(())
    }

    /// Sends an TCP ACK packet without payload.
    pub fn send_tcp_ack_0(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        let key = (src, dst);

        // TCP
        let state = self.states.get(&key).unwrap();
        let tcp = Tcp::new_ack(
            dst.port(),
            src.port(),
            state.sequence(),
            state.acknowledgement(),
            self.get_tcp_window(dst, src),
            state.sacks().clone(),
            None,
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), src.ip().clone(), Layers::Tcp(tcp), None)
    }

    fn send_tcp_ack_syn(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        let key = (src, dst);

        let mss = match ENABLE_MSS {
            true => {
                let mss = self.local_mtu - (Ipv4::minimum_len() + Tcp::minimum_len());
                let mss = if mss > u16::MAX as usize {
                    u16::MAX
                } else {
                    mss as u16
                };

                Some(mss)
            }
            false => None,
        };

        // TCP
        let state = self.states.get(&key).unwrap();
        let tcp = Tcp::new_ack_syn(
            dst.port(),
            src.port(),
            state.sequence(),
            state.acknowledgement(),
            self.get_tcp_window(dst, src),
            mss,
            state.send_wscale(),
            state.sack_perm(),
            None,
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), src.ip().clone(), Layers::Tcp(tcp), None)?;

        Ok(())
    }

    /// Sends an TCP ACK/RST packet.
    pub fn send_tcp_ack_rst(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        let key = (src, dst);

        // TCP
        let state = self.states.get(&key).unwrap();
        let tcp = Tcp::new_ack_rst(
            dst.port(),
            src.port(),
            state.sequence(),
            state.acknowledgement(),
            self.get_tcp_window(dst, src),
            None,
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), src.ip().clone(), Layers::Tcp(tcp), None)
    }

    /// Sends an TCP RST packet.
    pub fn send_tcp_rst(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        // TCP
        let tcp = Tcp::new_rst(dst.port(), src.port(), 0, 0, 0, None);

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), src.ip().clone(), Layers::Tcp(tcp), None)
    }

    fn send_tcp_fin(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        let key = (src, dst);

        // TCP
        let state = self.states.get(&key).unwrap();
        let tcp = Tcp::new_fin(
            dst.port(),
            src.port(),
            state.sequence(),
            state.acknowledgement(),
            self.get_tcp_window(dst, src),
            None,
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), src.ip().clone(), Layers::Tcp(tcp), None)
    }

    /// Sends UDP packets.
    pub fn send_udp(
        &mut self,
        dst: SocketAddrV4,
        src: SocketAddrV4,
        payload: &[u8],
    ) -> io::Result<()> {
        // Fragmentation
        let size = Udp::minimum_len() + payload.len();
        let mss = *self.src_mtu.get(src.ip()).unwrap_or(&self.local_mtu) - Ipv4::minimum_len();
        if size <= mss {
            // Send
            self.send_udp_raw(dst, src, payload)?;
        } else {
            // Fragmentation required
            // UDP
            let mut udp = Udp::new(dst.port(), src.port());
            let ipv4 = Ipv4::new(0, udp.kind(), dst.ip().clone(), src.ip().clone()).unwrap();
            udp.set_ipv4_layer(&ipv4);
            let udp = udp;

            // Payload
            let mut buffer = vec![0u8; udp.len() + payload.len()];
            udp.serialize_with_payload(buffer.as_mut_slice(), payload, udp.len() + payload.len())?;
            let buffer = buffer;

            let mut n = 0;
            while n < size {
                let mut length = min(size - n, mss);
                let mut remain = size - n - length;

                // Alignment
                if remain > 0 {
                    length = length / 8 * 8;
                    remain = size - n - length;
                }

                // Leave at least 8 Bytes for last fragment
                if remain > 0 && remain < 8 {
                    length = length - 8;
                }

                // Send
                if remain > 0 {
                    self.send_ipv4_with_fragment(
                        dst.ip().clone(),
                        src.ip().clone(),
                        udp.kind(),
                        (n / 8) as u16,
                        &buffer[n..n + length],
                    )?;
                } else {
                    self.send_ipv4_with_last_fragment(
                        dst.ip().clone(),
                        src.ip().clone(),
                        udp.kind(),
                        (n / 8) as u16,
                        &buffer[n..n + length],
                    )?;
                }

                n = n + length;
            }
        }

        Ok(())
    }

    fn send_udp_raw(
        &mut self,
        dst: SocketAddrV4,
        src: SocketAddrV4,
        payload: &[u8],
    ) -> io::Result<()> {
        // UDP
        let udp = Udp::new(dst.port(), src.port());

        self.send_ipv4_with_transport(
            dst.ip().clone(),
            src.ip().clone(),
            Layers::Udp(udp),
            Some(payload),
        )
    }

    fn send_ipv4_with_fragment(
        &mut self,
        dst_ip_addr: Ipv4Addr,
        src_ip_addr: Ipv4Addr,
        t: LayerKind,
        fragment_offset: u16,
        payload: &[u8],
    ) -> io::Result<()> {
        // IPv4
        let ipv4 = Ipv4::new_more_fragment(
            *self
                .ipv4_identification_map
                .get(&(src_ip_addr, dst_ip_addr))
                .unwrap_or(&0),
            t,
            fragment_offset,
            dst_ip_addr,
            src_ip_addr,
        )
        .unwrap();

        // Send
        self.send_ethernet(
            *self
                .src_hardware_addr
                .get(&src_ip_addr)
                .unwrap_or(&pcap::HARDWARE_ADDR_UNSPECIFIED),
            Layers::Ipv4(ipv4),
            None,
            Some(payload),
        )
    }

    fn send_ipv4_with_last_fragment(
        &mut self,
        dst_ip_addr: Ipv4Addr,
        src_ip_addr: Ipv4Addr,
        t: LayerKind,
        fragment_offset: u16,
        payload: &[u8],
    ) -> io::Result<()> {
        // IPv4
        let ipv4 = Ipv4::new_last_fragment(
            *self
                .ipv4_identification_map
                .get(&(src_ip_addr, dst_ip_addr))
                .unwrap_or(&0),
            t,
            fragment_offset,
            dst_ip_addr,
            src_ip_addr,
        )
        .unwrap();

        // Send
        self.send_ethernet(
            *self
                .src_hardware_addr
                .get(&src_ip_addr)
                .unwrap_or(&pcap::HARDWARE_ADDR_UNSPECIFIED),
            Layers::Ipv4(ipv4),
            None,
            Some(payload),
        )?;

        // Update IPv4 identification
        self.increase_ipv4_identification(dst_ip_addr, src_ip_addr);

        Ok(())
    }

    fn send_ipv4_with_transport(
        &mut self,
        dst_ip_addr: Ipv4Addr,
        src_ip_addr: Ipv4Addr,
        mut transport: Layers,
        payload: Option<&[u8]>,
    ) -> io::Result<()> {
        // IPv4
        let ipv4 = Ipv4::new(
            *self
                .ipv4_identification_map
                .get(&(src_ip_addr, dst_ip_addr))
                .unwrap_or(&0),
            transport.kind(),
            dst_ip_addr,
            src_ip_addr,
        )
        .unwrap();

        // Set IPv4 layer for checksum
        match transport {
            Layers::Tcp(ref mut tcp) => tcp.set_ipv4_layer(&ipv4),
            Layers::Udp(ref mut udp) => udp.set_ipv4_layer(&ipv4),
            _ => {}
        }

        // Send
        self.send_ethernet(
            *self
                .src_hardware_addr
                .get(&src_ip_addr)
                .unwrap_or(&pcap::HARDWARE_ADDR_UNSPECIFIED),
            Layers::Ipv4(ipv4),
            Some(transport),
            payload,
        )?;

        // Update IPv4 identification
        self.increase_ipv4_identification(dst_ip_addr, src_ip_addr);

        Ok(())
    }

    fn send_ethernet(
        &mut self,
        src_hardware_addr: HardwareAddr,
        network: Layers,
        transport: Option<Layers>,
        payload: Option<&[u8]>,
    ) -> io::Result<()> {
        // Ethernet
        let ethernet =
            Ethernet::new(network.kind(), self.local_hardware_addr, src_hardware_addr).unwrap();

        // Indicator
        let indicator = Indicator::new(Layers::Ethernet(ethernet), Some(network), transport);

        // Send
        match payload {
            Some(payload) => self.send_with_payload(&indicator, payload),
            None => self.send(&indicator),
        }
    }

    fn send(&mut self, indicator: &Indicator) -> io::Result<()> {
        // Serialize
        let size = indicator.len();
        let buffer_size = max(size, MINIMUM_FRAME_SIZE);
        let mut buffer = vec![0u8; buffer_size];
        indicator.serialize(&mut buffer[..size])?;

        // Send
        self.tx.send_to(&buffer, None).unwrap_or(Ok(()))?;
        debug!("send to pcap: {} ({} Bytes)", indicator.brief(), size);

        Ok(())
    }

    fn send_with_payload(&mut self, indicator: &Indicator, payload: &[u8]) -> io::Result<()> {
        // Serialize
        let size = indicator.len();
        let buffer_size = max(size + payload.len(), MINIMUM_FRAME_SIZE);
        let mut buffer = vec![0u8; buffer_size];
        indicator.serialize_with_payload(&mut buffer[..size + payload.len()], payload)?;

        // Send
        self.tx.send_to(&buffer, None).unwrap_or(Ok(()))?;
        debug!(
            "send to pcap: {} ({} + {} Bytes)",
            indicator.brief(),
            size,
            payload.len()
        );

        Ok(())
    }
}

impl ForwardStream for Forwarder {
    fn open(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        self.send_tcp_ack_syn(dst, src)?;

        let state = self.get_state(dst, src).unwrap();
        state.update_syn_timer();

        Ok(())
    }

    fn forward(&mut self, dst: SocketAddrV4, src: SocketAddrV4, payload: &[u8]) -> io::Result<()> {
        let key = (src, dst);

        let state = self.states.get(&key).unwrap();
        if state.cache_fin().is_some() || state.queue_fin() {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }

        self.append_to_queue(dst, src, payload)
    }

    fn tick(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        self.retransmit_tcp_ack_timedout(dst, src)
    }

    fn close(&mut self, dst: SocketAddrV4, src: SocketAddrV4) -> io::Result<()> {
        let state = self.get_state(dst, src).unwrap();
        state.append_queue_fin();

        self.send_tcp_ack(dst, src)
    }
}

impl ForwardDatagram for Forwarder {
    fn forward(&mut self, dst: SocketAddrV4, src: SocketAddrV4, payload: &[u8]) -> io::Result<()> {
        self.send_udp(dst, src, payload)
    }
}

fn disjoint_u32_range(main: (u32, u32), sub: (u32, u32)) -> Vec<(u32, u32)> {
    let size_main = main
        .1
        .checked_sub(main.0)
        .unwrap_or_else(|| main.1 + (u32::MAX - main.0)) as usize;
    let diff_first = sub
        .0
        .checked_sub(main.0)
        .unwrap_or_else(|| sub.0 + (u32::MAX - main.0)) as usize;
    let diff_second = sub
        .1
        .checked_sub(main.1)
        .unwrap_or_else(|| sub.1 + (u32::MAX - main.1)) as usize;
    let mut vector = Vec::with_capacity(2);

    if diff_first <= MAX_U32_WINDOW_SIZE {
        if diff_second > MAX_U32_WINDOW_SIZE {
            // sub is in the main
            vector.push((main.0, sub.0));
            vector.push((sub.1, main.1));
        } else {
            if diff_first >= size_main {
                // sub is in the right of the main
                vector.push((main.0, main.1));
            } else {
                // sub overlaps the right part of the main
                vector.push((main.0, sub.0));
            }
        }
    } else {
        if diff_second > MAX_U32_WINDOW_SIZE {
            // The distance between the main's left edge and the sub's right edge
            let diff = sub
                .1
                .checked_sub(main.0)
                .unwrap_or_else(|| sub.1 + (u32::MAX - main.0)) as usize;
            if diff > MAX_U32_WINDOW_SIZE {
                // sub is in the left of the main
                vector.push((main.0, main.1));
            } else {
                // sub overlaps the left part of the main
                vector.push((sub.1, main.1));
            }
        } else {
            // sub covers the main
        }
    }

    vector
}

/// Represents the threshold of TCP ACK duplicates before trigger a fast retransmission.
const DUPLICATES_THRESHOLD: usize = 3;
/// Represents the cool down time between 2 retransmissions.
const RETRANS_COOL_DOWN: u128 = 200;

/// Represents the RX state of a TCP connection.
struct TcpRxState {
    src: SocketAddrV4,
    dst: SocketAddrV4,
    recv_next: u32,
    last_acknowledgement: u32,
    duplicate: usize,
    last_retrans: Option<Instant>,
    wscale: u8,
    sack_perm: bool,
    cache: Window,
    fin_sequence: Option<u32>,
}

impl TcpRxState {
    /// Creates a new `TcpRxState`, the sequence is the sequence in the TCP SYN packet.
    fn new(
        src: SocketAddrV4,
        dst: SocketAddrV4,
        sequence: u32,
        wscale: u8,
        sack_perm: bool,
    ) -> TcpRxState {
        let recv_next = sequence.checked_add(1).unwrap_or(0);

        trace!("admit TCP SYN of {} -> {}", src, dst);

        TcpRxState {
            src,
            dst,
            recv_next,
            last_acknowledgement: 0,
            duplicate: 0,
            last_retrans: None,
            wscale,
            sack_perm,
            cache: Window::with_capacity((RECV_WINDOW as usize) << wscale as usize, recv_next),
            fin_sequence: None,
        }
    }

    fn add_recv_next(&mut self, n: u32) {
        self.recv_next = self
            .recv_next
            .checked_add(n)
            .unwrap_or_else(|| n - (u32::MAX - self.recv_next));
        trace!(
            "add TCP receive next of {} -> {} to {}",
            self.src,
            self.dst,
            self.recv_next
        );
    }

    /// Increases the duplication counter of the TCP connection and returns if a fast
    /// retransmission should be performed.
    fn increase_duplicate(&mut self, acknowledgement: u32) -> bool {
        if self.last_acknowledgement == acknowledgement {
            self.duplicate = self.duplicate.checked_add(1).unwrap_or(usize::MAX);
            trace!(
                "increase TCP duplicate of {} -> {} at {} to {}",
                self.src,
                self.dst,
                acknowledgement,
                self.duplicate
            );

            if self.duplicate >= DUPLICATES_THRESHOLD {
                let is_cooled_down = match self.last_retrans {
                    Some(ref instant) => instant.elapsed().as_millis() < RETRANS_COOL_DOWN,
                    None => false,
                };

                return !is_cooled_down;
            }
        } else {
            self.clear_duplicate();
            self.last_acknowledgement = acknowledgement;
        }

        false
    }

    fn clear_duplicate(&mut self) {
        self.duplicate = 0;
        trace!(
            "clear TCP duplicate of {} -> {} at {}",
            self.src,
            self.dst,
            self.last_acknowledgement
        );
    }

    fn set_last_retrans(&mut self) {
        self.last_retrans = Some(Instant::now());
        trace!(
            "set TCP last retransmission of {} -> {}",
            self.src,
            self.dst,
        );
    }

    fn append_cache(&mut self, sequence: u32, payload: &[u8]) -> io::Result<Option<Vec<u8>>> {
        trace!(
            "append {} Bytes to TCP cache of {} -> {}",
            payload.len(),
            self.src,
            self.dst
        );
        self.cache.append(sequence, payload)
    }

    fn set_fin_sequence(&mut self, sequence: u32) {
        self.fin_sequence = Some(sequence);
        trace!(
            "set TCP FIN sequence of {} -> {} to {}",
            self.src,
            self.dst,
            sequence
        );
    }

    fn admit_fin(&mut self) {
        self.fin_sequence = None;
        trace!("admit TCP FIN of {} -> {}", self.src, self.dst);
    }
}

impl Display for TcpRxState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TCP RX State: {} -> {}", self.src, self.dst)
    }
}

/// Represents if the TCP window scale option is enabled.
const ENABLE_WSCALE: bool = true;
/// Represents the max window scale of the receive window.
const MAX_RECV_WSCALE: u8 = 8;

/// Represents if the TCP selective acknowledgment option is enabled.
const ENABLE_SACK: bool = true;

/// Represents the max limit of UDP port for binding in local.
const MAX_UDP_PORT: usize = 256;

/// Represents a channel redirect traffic to the proxy of SOCKS or loopback to the source in pcap.
pub struct Redirector {
    tx: Arc<Mutex<Forwarder>>,
    is_tx_src_hardware_addr_set: bool,
    src_ip_addr: Ipv4Network,
    local_ip_addr: Ipv4Addr,
    gw_ip_addr: Option<Ipv4Addr>,
    remote: SocketAddrV4,
    options: SocksOption,
    streams: HashMap<(SocketAddrV4, SocketAddrV4), StreamWorker>,
    states: HashMap<(SocketAddrV4, SocketAddrV4), TcpRxState>,
    datagrams: HashMap<u16, DatagramWorker>,
    /// Represents the map mapping a source port to a local port.
    datagram_map: HashMap<SocketAddrV4, u16>,
    /// Represents the LRU mapping a local port to a source port.
    udp_lru: LruCache<u16, SocketAddrV4>,
    defrag: Defraggler,
}

impl Redirector {
    /// Creates a new `Redirector`.
    pub fn new(
        tx: Arc<Mutex<Forwarder>>,
        src_ip_addr: Ipv4Network,
        local_ip_addr: Ipv4Addr,
        gw_ip_addr: Option<Ipv4Addr>,
        remote: SocketAddrV4,
        force_associate_dst: bool,
        force_associate_bind_addr: bool,
        auth: Option<(String, String)>,
    ) -> Redirector {
        let auth = match auth {
            Some((username, password)) => Some(SocksAuth::new(username, password)),
            None => None,
        };
        let redirector = Redirector {
            tx,
            is_tx_src_hardware_addr_set: false,
            src_ip_addr,
            local_ip_addr,
            gw_ip_addr,
            remote,
            options: SocksOption::new(force_associate_dst, force_associate_bind_addr, auth),
            streams: HashMap::new(),
            states: HashMap::new(),
            datagrams: HashMap::new(),
            datagram_map: HashMap::new(),
            udp_lru: LruCache::new(MAX_UDP_PORT),
            defrag: Defraggler::new(),
        };
        if let Some(gw_ip_addr) = gw_ip_addr {
            redirector.tx.lock().unwrap().set_local_ip_addr(gw_ip_addr);
        }

        redirector
    }

    /// Opens an `Interface` for redirect.
    pub async fn open(&mut self, rx: &mut Receiver) -> io::Result<()> {
        loop {
            match rx.next() {
                Ok(frame) => {
                    if let Some(ref indicator) = Indicator::from(frame) {
                        if let Some(t) = indicator.network_kind() {
                            match t {
                                LayerKinds::Arp => {
                                    if let Err(ref e) = self.handle_arp(indicator) {
                                        warn!("handle {}: {}", indicator.brief(), e);
                                    }
                                }
                                LayerKinds::Ipv4 => {
                                    if let Err(ref e) = self.handle_ipv4(indicator, frame).await {
                                        warn!("handle {}: {}", indicator.brief(), e);
                                    }
                                }
                                _ => unreachable!(),
                            }
                        }
                    };
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::TimedOut {
                        thread::sleep(Duration::from_millis(TIMEDOUT_WAIT));
                        continue;
                    }
                    return Err(e);
                }
            };
        }
    }

    fn handle_arp(&mut self, indicator: &Indicator) -> io::Result<()> {
        if let Some(gw_ip_addr) = self.gw_ip_addr {
            if let Some(arp) = indicator.arp() {
                let src = arp.src();
                if src != self.local_ip_addr
                    && self.src_ip_addr.contains(src)
                    && arp.dst() == gw_ip_addr
                {
                    debug!(
                        "receive from pcap: {} ({} Bytes)",
                        indicator.brief(),
                        indicator.len()
                    );

                    // Set forwarder's hardware address
                    if !self.is_tx_src_hardware_addr_set {
                        self.tx
                            .lock()
                            .unwrap()
                            .set_src_hardware_addr(src, arp.src_hardware_addr());
                        self.is_tx_src_hardware_addr_set = true;
                        info!(
                            "Device {} ({}) joined the network",
                            src,
                            arp.src_hardware_addr()
                        );
                    }

                    // Send
                    self.tx.lock().unwrap().send_arp_reply(src)?
                }
            }
        }

        Ok(())
    }

    async fn handle_ipv4(&mut self, indicator: &Indicator, frame: &[u8]) -> io::Result<()> {
        if let Some(ipv4) = indicator.ipv4() {
            let src = ipv4.src();
            if src != self.local_ip_addr && self.src_ip_addr.contains(src) {
                debug!(
                    "receive from pcap: {} ({} + {} Bytes)",
                    indicator.brief(),
                    indicator.len(),
                    indicator.content_len() - indicator.len()
                );
                // Set forwarder's hardware address
                if !self.is_tx_src_hardware_addr_set {
                    self.tx
                        .lock()
                        .unwrap()
                        .set_src_hardware_addr(src, indicator.ethernet().unwrap().src());
                    self.is_tx_src_hardware_addr_set = true;
                    info!(
                        "Device {} joined the network",
                        indicator.ethernet().unwrap().src()
                    );
                }

                let frame_without_padding = &frame[..indicator.content_len()];
                if ipv4.is_fragment() {
                    // Fragmentation
                    let frag = match self.defrag.add(indicator, frame_without_padding) {
                        Some(frag) => frag,
                        None => return Ok(()),
                    };
                    let (transport, payload) = frag.concatenate();

                    if let Some(transport) = transport {
                        match transport {
                            Layers::Icmpv4(ref icmpv4) => self.handle_icmpv4(icmpv4)?,
                            Layers::Tcp(ref tcp) => self.handle_tcp(tcp, &payload).await?,
                            Layers::Udp(ref udp) => self.handle_udp(udp, &payload).await?,
                            _ => unreachable!(),
                        }
                    }
                } else {
                    if let Some(transport) = indicator.transport() {
                        match transport {
                            Layers::Icmpv4(icmpv4) => self.handle_icmpv4(icmpv4)?,
                            Layers::Tcp(tcp) => {
                                self.handle_tcp(tcp, &frame_without_padding[indicator.len()..])
                                    .await?
                            }
                            Layers::Udp(udp) => {
                                self.handle_udp(udp, &frame_without_padding[indicator.len()..])
                                    .await?
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_icmpv4(&mut self, icmpv4: &Icmpv4) -> io::Result<()> {
        if icmpv4.is_destination_port_unreachable() {
            // Destination port unreachable
            let kind = match icmpv4.next_level_layer_kind() {
                Some(kind) => kind,
                None => return Ok(()),
            };
            match kind {
                LayerKinds::Udp => {
                    let dst = icmpv4.dst().unwrap();
                    self.unbind_local_udp_port(dst);
                }
                _ => {}
            }
        } else if icmpv4.is_fragmentation_required_and_df_flag_set() {
            // Fragmentation required, and DF flag set
            let mtu = icmpv4.next_hop_mtu().unwrap();
            if self
                .tx
                .lock()
                .unwrap()
                .set_src_mtu(icmpv4.dst_ip_addr().unwrap(), mtu as usize)
            {
                info!("Update MTU of {} to {}", icmpv4.dst_ip_addr().unwrap(), mtu);
            }
        }

        Ok(())
    }

    async fn handle_tcp(&mut self, tcp: &Tcp, payload: &[u8]) -> io::Result<()> {
        if tcp.is_rst() {
            self.handle_tcp_rst(tcp);
        } else if tcp.is_ack() {
            self.handle_tcp_ack(tcp, payload).await?;
        } else if tcp.is_syn() {
            // Pure TCP SYN
            self.handle_tcp_syn(tcp).await?;
        } else if tcp.is_fin() {
            // Pure TCP FIN
            self.handle_tcp_fin(tcp, payload)?;
        } else {
            unreachable!();
        }

        Ok(())
    }

    async fn handle_tcp_ack(&mut self, tcp: &Tcp, payload: &[u8]) -> io::Result<()> {
        let src = SocketAddrV4::new(tcp.src_ip_addr(), tcp.src());
        let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
        let key = (src, dst);
        let is_exist = self.streams.get(&key).is_some();
        let is_writable = match self.streams.get(&key) {
            Some(stream) => !stream.is_write_closed(),
            None => false,
        };

        if is_exist {
            // ACK
            let state = self.states.get_mut(&key).unwrap();
            if tcp.sequence() != state.recv_next {
                trace!(
                    "TCP out of order of {} -> {} at {}",
                    src,
                    dst,
                    tcp.sequence()
                );
            }
            {
                let mut tx_locked = self.tx.lock().unwrap();
                let tx_state = tx_locked.get_state(dst, src).unwrap();

                tx_state.acknowledge(tcp.acknowledgement());
                tx_state.set_send_window((tcp.window() as usize) << state.wscale as usize);
            }

            if payload.len() > 0 {
                // ACK
                // Append to cache
                let cont_payload = state.append_cache(tcp.sequence(), payload)?;

                // SACK
                if state.sack_perm {
                    let sacks = state.cache.filled();
                    self.tx
                        .lock()
                        .unwrap()
                        .get_state(dst, src)
                        .unwrap()
                        .set_sacks(&sacks);
                }

                match cont_payload {
                    Some(payload) => {
                        // Send
                        let stream = self.streams.get_mut(&key).unwrap();
                        match stream.send(payload.as_slice()).await {
                            Ok(_) => {
                                let cache_remaining_size =
                                    (state.cache.remaining() >> state.wscale as usize) as u16;

                                state.add_recv_next(payload.len() as u32);

                                let mut tx_locked = self.tx.lock().unwrap();
                                let tx_state = tx_locked.get_state(dst, src).unwrap();

                                // Update window size
                                tx_state.set_window(cache_remaining_size);

                                // Update TCP acknowledgement
                                tx_state.add_acknowledgement(payload.len() as u32);

                                // Send ACK0
                                // If there is a heavy traffic, the ACK reported may be inaccurate, which would results in retransmission
                                tx_locked.send_tcp_ack_0(dst, src)?;
                            }
                            Err(e) => {
                                {
                                    // Send ACK/RST
                                    let mut tx_locked = self.tx.lock().unwrap();

                                    tx_locked.send_tcp_ack_rst(dst, src)?;
                                }

                                // Clean up
                                self.clean_up(src, dst);

                                return Err(e);
                            }
                        }
                    }
                    None => {
                        // Retransmission or unordered
                        let cache_remaining_size =
                            (state.cache.remaining() >> state.wscale as usize) as u16;

                        // Update window size
                        let mut tx_locked = self.tx.lock().unwrap();
                        let tx_state = tx_locked.get_state(dst, src).unwrap();

                        tx_state.set_window(cache_remaining_size);

                        // Send ACK0
                        tx_locked.send_tcp_ack_0(dst, src)?;
                    }
                }
            } else {
                // ACK0
                if !is_writable && self.tx.lock().unwrap().get_cache_size(dst, src) == 0 {
                    // LAST_ACK
                    // Clean up
                    self.streams.remove(&key);
                    self.states.remove(&key);
                    self.tx.lock().unwrap().clean_up(dst, src);

                    return Ok(());
                } else {
                    let is_retrans = state.increase_duplicate(tcp.acknowledgement());
                    // Duplicate ACK
                    if is_retrans && !tcp.is_zero_window() {
                        // Fast retransmit
                        let mut is_sr = false;
                        if state.sack_perm {
                            if let Some(sacks) = tcp.sack() {
                                if sacks.len() > 0 {
                                    // Selective retransmission
                                    self.tx
                                        .lock()
                                        .unwrap()
                                        .retransmit_tcp_ack_without(dst, src, sacks)?;
                                    is_sr = true;
                                }
                            }
                        }

                        if !is_sr {
                            // Back N
                            self.tx.lock().unwrap().retransmit_tcp_ack(dst, src)?;
                        }

                        state.clear_duplicate();
                        state.set_last_retrans();
                    }
                }
            }

            // Trigger sending remaining data
            self.tx.lock().unwrap().send_tcp_ack(dst, src)?;

            // FIN
            if tcp.is_fin() || state.fin_sequence.is_some() {
                self.handle_tcp_fin(tcp, payload)?;
            }
        } else {
            // Send RST
            self.tx.lock().unwrap().send_tcp_rst(dst, src)?;
        }

        Ok(())
    }

    async fn handle_tcp_syn(&mut self, tcp: &Tcp) -> io::Result<()> {
        let src = SocketAddrV4::new(tcp.src_ip_addr(), tcp.src());
        let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
        let key = (src, dst);
        let is_exist = self.streams.get(&key).is_some();

        // Connect if not connected, drop if established
        if !is_exist {
            // Clean up
            self.clean_up(src, dst);

            // Admit SYN
            let wscale = match ENABLE_WSCALE {
                true => tcp.wscale(),
                false => None,
            };
            let recv_wscale = match wscale {
                Some(wscale) => Some(min(wscale, MAX_RECV_WSCALE)),
                None => None,
            };
            let sack_perm = ENABLE_SACK && tcp.is_sack_perm();
            let state = TcpRxState::new(src, dst, tcp.sequence(), wscale.unwrap_or(0), sack_perm);

            {
                let mut tx_locked = self.tx.lock().unwrap();

                let mut rng = rand::thread_rng();
                let sequence = rng.gen::<u32>();
                let acknowledgement = tcp.sequence().checked_add(1).unwrap_or(0);
                if let Some(mss) = tcp.mss() {
                    let mtu = Ipv4::minimum_len() + Tcp::minimum_len() + mss as usize;
                    if tx_locked.set_src_mtu(tcp.src_ip_addr(), mtu) {
                        info!("Update MTU of {} to {}", tcp.src_ip_addr(), mtu);
                    }
                }

                let tx_state = TcpTxState::new(
                    src,
                    dst,
                    sequence,
                    acknowledgement,
                    tcp.window(),
                    recv_wscale,
                    sack_perm,
                    wscale,
                );
                tx_locked.set_state(dst, src, tx_state);
            }

            // Connect
            let stream =
                StreamWorker::connect(self.get_tx(), src, dst, self.remote, &self.options).await;

            let stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    {
                        let mut tx_locked = self.tx.lock().unwrap();
                        let tx_state = tx_locked.get_state(dst, src).unwrap();

                        tx_state.add_acknowledgement(1);

                        // Send ACK/RST
                        tx_locked.send_tcp_ack_rst(dst, src)?;
                    }

                    // Clean up
                    self.clean_up(src, dst);

                    return Err(e);
                }
            };

            self.states.insert(key, state);
            self.streams.insert(key, stream);
        }

        Ok(())
    }

    fn handle_tcp_rst(&mut self, tcp: &Tcp) {
        let src = SocketAddrV4::new(tcp.src_ip_addr(), tcp.src());
        let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());

        // Clean up
        self.clean_up(src, dst);
    }

    fn handle_tcp_fin(&mut self, tcp: &Tcp, payload: &[u8]) -> io::Result<()> {
        let src = SocketAddrV4::new(tcp.src_ip_addr(), tcp.src());
        let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
        let key = (src, dst);
        let is_exist = self.streams.get(&key).is_some();
        let is_readable = match self.streams.get(&key) {
            Some(stream) => !stream.is_read_closed(),
            None => false,
        };

        if is_exist {
            let state = self.states.get_mut(&key).unwrap();
            if tcp.is_fin() {
                // Update FIN sequence
                state.set_fin_sequence(
                    tcp.sequence()
                        .checked_add(payload.len() as u32)
                        .unwrap_or_else(|| payload.len() as u32 - (u32::MAX - tcp.sequence())),
                );
            }

            // If the receive next is the same as the FIN sequence, the FIN should be popped
            if let Some(fin_sequence) = state.fin_sequence {
                if fin_sequence == state.recv_next {
                    // Admit FIN
                    state.admit_fin();
                    state.add_recv_next(1);

                    {
                        let mut tx_locked = self.tx.lock().unwrap();
                        let tx_state = tx_locked.get_state(dst, src).unwrap();

                        tx_state.add_acknowledgement(1);

                        // Send ACK0
                        tx_locked.send_tcp_ack_0(dst, src)?;
                    }
                    if is_readable {
                        // Close by local
                        let stream = self.streams.get_mut(&key).unwrap();
                        stream.shutdown(Shutdown::Write);
                    } else {
                        // Close by remote
                        // Clean up
                        self.clean_up(src, dst);
                    }
                } else {
                    trace!(
                        "TCP out of order of {} -> {} at {}",
                        src,
                        dst,
                        tcp.sequence()
                    );

                    if payload.len() == 0 {
                        // Send ACK0
                        self.tx.lock().unwrap().send_tcp_ack_0(dst, src)?;
                    }
                }
            }
        } else {
            // Send RST
            self.tx.lock().unwrap().send_tcp_rst(dst, src)?;
        }

        Ok(())
    }

    fn clean_up(&mut self, src: SocketAddrV4, dst: SocketAddrV4) {
        let key = (src, dst);

        self.streams.remove(&key);
        self.states.remove(&key);

        self.tx.lock().unwrap().clean_up(dst, src);
    }

    async fn handle_udp(&mut self, udp: &Udp, payload: &[u8]) -> io::Result<()> {
        let src = SocketAddrV4::new(udp.src_ip_addr(), udp.src());

        // Bind
        let port = self.bind_local_udp_port(src).await?;

        // Send
        self.datagrams
            .get_mut(&port)
            .unwrap()
            .send_to(payload, SocketAddrV4::new(udp.dst_ip_addr(), udp.dst()))
            .await?;

        Ok(())
    }

    async fn bind_local_udp_port(&mut self, src: SocketAddrV4) -> io::Result<u16> {
        let local_port = self.datagram_map.get(&src);
        match local_port {
            Some(&local_port) => {
                // Update LRU
                self.udp_lru.get(&local_port);

                Ok(local_port)
            }
            None => {
                let bind_port = if self.udp_lru.len() < self.udp_lru.cap() {
                    match DatagramWorker::bind(self.get_tx(), src, self.remote, &self.options).await
                    {
                        Ok((worker, port)) => {
                            self.datagrams.insert(port, worker);

                            // Update map and LRU
                            self.datagram_map.insert(src, port);
                            self.udp_lru.put(port, src);

                            trace!("bind UDP port {} = {}", port, src);

                            Ok(port)
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err(io::Error::new(io::ErrorKind::Other, "cannot bind UDP port"))
                };

                match bind_port {
                    Ok(port) => Ok(port),
                    Err(e) => {
                        if self.udp_lru.is_empty() {
                            Err(e)
                        } else {
                            let pair = self.udp_lru.pop_lru().unwrap();
                            let port = pair.0;
                            let prev_src = pair.1;

                            // Reuse
                            self.datagram_map.remove(&prev_src);
                            trace!("reuse UDP port {} = {} to {}", port, prev_src, src);
                            self.datagram_map.insert(src.clone(), port);

                            // Update LRU
                            self.udp_lru.put(port, src.clone());

                            Ok(port)
                        }
                    }
                }
            }
        }
    }

    fn unbind_local_udp_port(&mut self, src: SocketAddrV4) {
        let local_port = self.datagram_map.get(&src);
        match local_port {
            Some(&local_port) => {
                self.datagrams.remove(&local_port);
                self.udp_lru.pop(&local_port);
                self.datagram_map.remove(&src);

                trace!("unbind UDP port {} = {}", local_port, src);
            }
            None => {}
        }
    }

    fn get_tx(&self) -> Arc<Mutex<Forwarder>> {
        Arc::clone(&self.tx)
    }
}

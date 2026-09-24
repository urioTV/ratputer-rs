//! IPv4 stack (embassy-net: DHCP + DNS + UDP) over the esp-radio station interface,
//! plus a one-shot SNTP query.
//!
//! There is no async executor: the stack runner and the SNTP job are futures
//! polled once per main-loop iteration with a no-op waker. That never blocks,
//! so DHCP/DNS/NTP progress in the background while the UI keeps rendering.
//! Timers inside embassy-net are served by the esp-rtos embassy time driver.

use alloc::boxed::Box;
use alloc::string::String;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Config, IpAddress, IpEndpoint, Ipv4Address, Stack, StackResources};
use embassy_time::{with_timeout, Duration};
use esp_radio::wifi::Interface;

const NTP_PORT: u16 = 123;
// Seconds between the NTP era (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
// time.cloudflare.com anycast, used when DNS resolution fails.
const FALLBACK_NTP_SERVER: Ipv4Address = Ipv4Address::new(162, 159, 200, 1);
const DNS_TIMEOUT: Duration = Duration::from_secs(4);
const SNTP_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug)]
pub enum SntpError {
    Timeout,
    Socket,
    Send,
    BadReply,
}

type SntpJob = Pin<Box<dyn Future<Output = Result<u64, SntpError>>>>;

pub struct Network {
    stack: Stack<'static>,
    runner: Pin<Box<dyn Future<Output = ()>>>,
    sntp: Option<SntpJob>,
}

impl Network {
    pub fn new(interface: Interface, random_seed: u64) -> Self {
        // DHCP/DNS/SNTP plus persistent FTP control + data TCP sockets.
        // Keep spare slots for a DNS query while the FTP sockets exist.
        let resources = Box::leak(Box::new(StackResources::<8>::new()));
        let (stack, runner) = embassy_net::new(
            interface,
            Config::dhcpv4(Default::default()),
            resources,
            random_seed,
        );
        let runner = Box::leak(Box::new(runner));
        Self {
            stack,
            runner: Box::pin(async move { runner.run().await }),
            sntp: None,
        }
    }

    pub fn stack(&self) -> Stack<'static> {
        self.stack
    }

    /// Advance only the network driver. FTP burst-polls this without advancing
    /// (and accidentally consuming) an SNTP completion result.
    pub fn poll_stack(&mut self) {
        let mut context = Context::from_waker(Waker::noop());
        let _ = self.runner.as_mut().poll(&mut context);
    }

    /// Advance the stack and the SNTP job without blocking. Returns the SNTP
    /// result (Unix seconds) in the iteration the job finishes.
    pub fn poll(&mut self) -> Option<Result<u64, SntpError>> {
        self.poll_stack();
        let mut context = Context::from_waker(Waker::noop());
        let job = self.sntp.as_mut()?;
        match job.as_mut().poll(&mut context) {
            Poll::Ready(result) => {
                self.sntp = None;
                Some(result)
            }
            Poll::Pending => None,
        }
    }

    pub fn is_link_up(&self) -> bool {
        self.stack.is_link_up()
    }

    /// Link up and DHCP lease acquired.
    pub fn is_online(&self) -> bool {
        self.stack.is_link_up() && self.stack.is_config_up()
    }

    pub fn sntp_running(&self) -> bool {
        self.sntp.is_some()
    }

    pub fn start_sntp(&mut self, server: String) {
        let stack = self.stack;
        self.sntp = Some(Box::pin(async move {
            with_timeout(SNTP_TIMEOUT, sntp_query(stack, server))
                .await
                .unwrap_or(Err(SntpError::Timeout))
        }));
    }
}

async fn sntp_query(stack: Stack<'static>, server: String) -> Result<u64, SntpError> {
    let address = match with_timeout(DNS_TIMEOUT, stack.dns_query(&server, DnsQueryType::A)).await {
        Ok(Ok(addresses)) if !addresses.is_empty() => addresses[0],
        _ => {
            log::warn!("DNS lookup of {server} failed, using fallback NTP server");
            IpAddress::Ipv4(FALLBACK_NTP_SERVER)
        }
    };

    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buffer = [0_u8; 128];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_buffer = [0_u8; 128];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    socket.bind(0).map_err(|_| SntpError::Socket)?;

    // SNTP client request: LI = 0, version 3, mode 3 (client).
    let mut request = [0_u8; 48];
    request[0] = 0x1B;
    let endpoint = IpEndpoint::new(address, NTP_PORT);
    socket
        .send_to(&request, endpoint)
        .await
        .map_err(|_| SntpError::Send)?;

    loop {
        let reply = socket
            .recv_from_with(|data, metadata| {
                (metadata.endpoint == endpoint).then(|| parse_reply(data))
            })
            .await;
        // Ignore stray datagrams from other hosts; keep waiting for the server.
        if let Some(result) = reply {
            return result;
        }
    }
}

fn parse_reply(data: &[u8]) -> Result<u64, SntpError> {
    if data.len() < 48 {
        return Err(SntpError::BadReply);
    }
    let mode = data[0] & 0x07;
    let stratum = data[1];
    // Mode 4 = server; stratum 0 is a "kiss-o'-death" refusal.
    if mode != 4 || stratum == 0 {
        return Err(SntpError::BadReply);
    }
    // Transmit timestamp, integer seconds since 1900 (big-endian).
    let seconds = u64::from(u32::from_be_bytes([data[40], data[41], data[42], data[43]]));
    seconds
        .checked_sub(NTP_UNIX_OFFSET)
        .ok_or(SntpError::BadReply)
}

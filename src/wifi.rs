use alloc::{string::String, vec::Vec};

use embassy_futures::block_on;
use embassy_time::{with_timeout, Duration as EmbassyDuration};
use esp_hal::time::Duration;
use esp_radio::wifi::{
    scan::{ScanConfig, ScanTypeConfig},
    sta::StationConfig,
    AuthenticationMethod, AuthenticationMethodConfig, Config, ConnectionError, Interface, Password,
    Ssid, WifiController, WifiError,
};

use crate::storage::{AuthKind, SavedNetwork};

const MAX_SCAN_RESULTS: usize = 8;
pub const SCAN_PASSES: usize = 2;
pub const CONNECT_ATTEMPTS: usize = 3;
const CONNECT_TIMEOUT: EmbassyDuration = EmbassyDuration::from_secs(12);
const DISCONNECT_TIMEOUT: EmbassyDuration = EmbassyDuration::from_secs(3);
const SCAN_TIMEOUT: EmbassyDuration = EmbassyDuration::from_secs(8);

#[derive(Clone, Debug)]
pub struct ScanNetwork {
    pub ssid: String,
    pub signal_strength: i8,
    pub auth: AuthKind,
    pub supported: bool,
}

pub struct WifiManager<'d> {
    controller: WifiController<'d>,
}

#[derive(Debug)]
pub enum ConnectError {
    InvalidCredentials,
    Radio,
    Connection,
}

impl From<WifiError> for ConnectError {
    fn from(_error: WifiError) -> Self {
        Self::Radio
    }
}

impl From<ConnectionError> for ConnectError {
    fn from(_error: ConnectionError) -> Self {
        Self::Connection
    }
}

impl<'d> WifiManager<'d> {
    /// Returns the controller plus the station interface, which the caller hands
    /// to the IP stack (see `net.rs`).
    pub fn new(device: esp_hal::peripherals::WIFI<'d>) -> Result<(Self, Interface), WifiError> {
        let controller = WifiController::new(device, Default::default())?;
        let interface = Interface::station();
        Ok((Self { controller }, interface))
    }

    /// A single scan pass. The caller repeats and merges passes so the UI can show
    /// progress between them (each pass blocks for up to a few seconds).
    pub fn scan_pass(&mut self) -> Result<Vec<ScanNetwork>, WifiError> {
        // The default active scan dwells only 10-20 ms per channel, which regularly
        // misses APs, so we extend the dwell on every pass.
        let config = ScanConfig::default()
            .with_max(MAX_SCAN_RESULTS)
            .with_scan_type(ScanTypeConfig::Active {
                min: Duration::from_millis(50),
                max: Duration::from_millis(250),
            });
        // The driver normally posts ScanDone within a few seconds. If it
        // doesn't (for example after repeated failed associations), never
        // leave the entire UI stuck inside block_on indefinitely.
        let results = match block_on(with_timeout(
            SCAN_TIMEOUT,
            self.controller.scan_async(&config),
        )) {
            Ok(result) => result?,
            Err(_) => {
                log::warn!("Wi-Fi scan timed out");
                return Err(WifiError::Other);
            }
        };
        Ok(results
            .into_iter()
            .filter_map(|access_point| {
                let ssid = access_point.ssid.as_str();
                if ssid.is_empty() {
                    return None;
                }
                let (auth, supported) = map_scanned_auth(access_point.auth_method);
                Some(ScanNetwork {
                    ssid: String::from(ssid),
                    signal_strength: access_point.signal_strength,
                    auth,
                    supported,
                })
            })
            .collect())
    }

    /// Apply station credentials once. Association itself is done via repeated
    /// [`connect_attempt`] calls so the UI can report attempt progress.
    pub fn configure(
        &mut self,
        ssid: &str,
        password: &str,
        auth: AuthKind,
    ) -> Result<(), ConnectError> {
        block_on(self.configure_async(ssid, password, auth))
    }

    async fn configure_async(
        &mut self,
        ssid: &str,
        password: &str,
        auth: AuthKind,
    ) -> Result<(), ConnectError> {
        let ssid = Ssid::try_from(ssid).map_err(|_| ConnectError::InvalidCredentials)?;
        let authentication = match auth {
            AuthKind::Open => AuthenticationMethodConfig::Open,
            AuthKind::Wep => AuthenticationMethodConfig::Wep(parse_password(password)?),
            AuthKind::Wpa => AuthenticationMethodConfig::Wpa(parse_password(password)?),
            AuthKind::Wpa2 => AuthenticationMethodConfig::Wpa2Personal(parse_password(password)?),
            AuthKind::WpaWpa2 => {
                AuthenticationMethodConfig::WpaWpa2Personal(parse_password(password)?)
            }
        };
        let station = StationConfig::default()
            .with_ssid(ssid)
            .with_authentication(authentication);

        let _ = with_timeout(DISCONNECT_TIMEOUT, self.controller.disconnect_async()).await;
        self.controller.set_config(&Config::Station(station))?;
        Ok(())
    }

    /// Single association attempt. Flaky on busy channels, so callers retry it.
    pub fn connect_attempt(&mut self) -> Result<(), ConnectError> {
        block_on(self.connect_attempt_async())
    }

    async fn connect_attempt_async(&mut self) -> Result<(), ConnectError> {
        match with_timeout(CONNECT_TIMEOUT, self.controller.connect_async()).await {
            Ok(Ok(_)) => Ok(()),
            result => {
                log::warn!("Wi-Fi connect attempt failed or timed out: {result:?}");
                let _ = with_timeout(DISCONNECT_TIMEOUT, self.controller.disconnect_async()).await;
                match result {
                    Ok(Err(error)) => Err(ConnectError::from(error)),
                    _ => Err(ConnectError::Connection),
                }
            }
        }
    }

    /// Own the radio for the whole automatic job; returning it on both success
    /// and failure lets the main loop poll this future without self-borrows.
    pub async fn connect_automatically(
        mut self,
        network: SavedNetwork,
    ) -> (Self, Result<(), ConnectError>) {
        let mut result = self
            .configure_async(&network.ssid, &network.password, network.auth)
            .await;
        if result.is_ok() {
            for _ in 0..CONNECT_ATTEMPTS {
                result = self.connect_attempt_async().await;
                if result.is_ok() {
                    break;
                }
            }
        }
        (self, result)
    }
}

/// Merge one scan pass into the accumulated list: dedupe by SSID, keep the
/// strongest signal, sort by signal strength, and cap the list length.
pub fn merge_scan_results(networks: &mut Vec<ScanNetwork>, results: Vec<ScanNetwork>) {
    for network in results {
        match networks.iter_mut().find(|entry| entry.ssid == network.ssid) {
            Some(existing) => {
                existing.signal_strength = existing.signal_strength.max(network.signal_strength)
            }
            None => networks.push(network),
        }
    }
    networks.sort_unstable_by(|left, right| right.signal_strength.cmp(&left.signal_strength));
    networks.truncate(MAX_SCAN_RESULTS);
}

fn parse_password(password: &str) -> Result<Password, ConnectError> {
    Password::try_from(password).map_err(|_| ConnectError::InvalidCredentials)
}

fn map_scanned_auth(method: Option<AuthenticationMethod>) -> (AuthKind, bool) {
    match method {
        Some(AuthenticationMethod::None) => (AuthKind::Open, true),
        Some(AuthenticationMethod::Wep) => (AuthKind::Wep, true),
        Some(AuthenticationMethod::Wpa) => (AuthKind::Wpa, true),
        Some(AuthenticationMethod::Wpa2Personal) => (AuthKind::Wpa2, true),
        Some(AuthenticationMethod::WpaWpa2Personal) => (AuthKind::WpaWpa2, true),
        _ => (AuthKind::Wpa2, false),
    }
}

//! Bluetooth PAN tethering module.
//!
//! Manages Bluetooth Personal Area Network (PAN) connections for
//! internet tethering via a paired phone.
//!
//! Uses D-Bus (via `dbus.rs`) for PAN connection and `bluetoothctl` for
//! adapter control. All Command calls are `#[cfg(unix)]` gated.

pub mod adapter;
pub mod attacks;
pub mod capture;
pub mod coex;
pub mod controller;
pub mod dbus;
pub mod discovery;
pub mod model;
pub mod patchram;
pub mod persistence;
pub mod supervisor;
pub mod ui;

use log::{info, warn};
use std::time::Instant;

/// Bluetooth connection states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtState {
    /// Bluetooth is off or adapter not found.
    Off,
    /// Adapter is on but not connected.
    Disconnected,
    /// Pairing/trusting a device before connecting.
    Pairing,
    /// Attempting to connect to a paired device.
    Connecting,
    /// Connected via PAN, internet available.
    Connected,
    /// Connection failed, will retry.
    Error,
}

/// Configuration for Bluetooth tethering.
#[derive(Debug, Clone)]
pub struct BtConfig {
    /// Whether Bluetooth tethering is enabled.
    pub enabled: bool,
    /// Display name of the phone (used for scan matching).
    pub phone_name: String,
    /// Whether to auto-connect on boot.
    pub auto_connect: bool,
    /// Whether to hide BT discoverability after connecting.
    pub hide_after_connect: bool,
}

impl Default for BtConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            phone_name: String::new(),
            auto_connect: true,
            hide_after_connect: true,
        }
    }
}

/// Reconnect backoff schedule (seconds).
const BACKOFF_SCHEDULE: &[u64] = &[30, 60, 120, 300];

/// Max backoff interval (seconds).
const BACKOFF_CAP: u64 = 300;

/// How long to poll for NAP UUID after pairing (seconds).
const NAP_WAIT_SECS: u64 = 15;

/// How many times to retry ConnectProfile on PhoneBusy.
const BUSY_RETRIES: u32 = 3;

/// Delay between PhoneBusy retries (seconds).
const BUSY_RETRY_DELAY_SECS: u64 = 3;

// ---------------------------------------------------------------------------
// Command builders (pure functions, testable on any platform)
// ---------------------------------------------------------------------------

/// Build the bluetoothctl command args to power on the adapter.
pub fn build_power_on_args() -> Vec<String> {
    vec!["power".into(), "on".into()]
}

/// Build the bluetoothctl command args to power off the adapter.
pub fn build_power_off_args() -> Vec<String> {
    vec!["power".into(), "off".into()]
}

/// Build the bluetoothctl command args to enable the agent.
pub fn build_agent_on_args() -> Vec<String> {
    vec!["agent".into(), "on".into()]
}

/// Build the bluetoothctl command args to set the default agent.
pub fn build_default_agent_args() -> Vec<String> {
    vec!["default-agent".into()]
}

/// Build the bluetoothctl command args to pair with a device.
pub fn build_pair_args(mac: &str) -> Vec<String> {
    vec!["pair".into(), mac.into()]
}

/// Build the bluetoothctl command args to trust a device.
pub fn build_trust_args(mac: &str) -> Vec<String> {
    vec!["trust".into(), mac.into()]
}

/// Build the bluetoothctl command args to turn off discoverability.
pub fn build_discoverable_off_args() -> Vec<String> {
    vec!["discoverable".into(), "off".into()]
}

/// Build the bluetoothctl command args to turn on discoverability.
pub fn build_discoverable_on_args() -> Vec<String> {
    vec!["discoverable".into(), "on".into()]
}

/// Build the bluetoothctl command args to scan for devices (with timeout).
pub fn build_scan_on_args() -> Vec<String> {
    vec!["--timeout".into(), "10".into(), "scan".into(), "on".into()]
}

/// Strip ANSI escape codes from a string.
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false; // end of escape sequence
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Parse ALL discovered devices from bluetoothctl scan output.
/// Returns a list of (MAC, name) pairs.
pub fn parse_scan_all_devices(output: &str) -> Vec<(String, String)> {
    let mut devices = Vec::new();
    for raw_line in output.lines() {
        let line = strip_ansi(raw_line);
        if let Some(rest) = line.strip_prefix("[NEW] Device ") {
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if !parts.is_empty() {
                let mac = parts[0].to_string();
                let name = if parts.len() >= 2 {
                    parts[1].to_string()
                } else {
                    String::new()
                };
                // Skip entries with no name or where name looks like a MAC address
                if !name.is_empty() && name != mac && !is_mac_like(&name) {
                    devices.push((mac, name));
                }
            }
        }
    }
    devices
}

/// Check if a string looks like a MAC address (e.g., "AA-BB-CC-DD-EE-FF" or "AA:BB:CC:DD:EE:FF").
fn is_mac_like(s: &str) -> bool {
    // MAC addresses are 17 chars: XX:XX:XX:XX:XX:XX or XX-XX-XX-XX-XX-XX
    if s.len() != 17 {
        return false;
    }
    let sep = if s.contains(':') { ':' } else { '-' };
    s.split(sep).count() == 6
        && s.split(sep)
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Parse an IPv4 address from `ip -4 addr show <iface>` output.
///
/// Looks for a line like `inet 192.168.44.128/24 ...` and extracts the IP.
pub fn parse_ip_from_output(output: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("inet ") {
            if let Some(addr_cidr) = rest.split_whitespace().next() {
                if let Some(addr) = addr_cidr.split('/').next() {
                    return Some(addr.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// System command execution (unix-only)
// ---------------------------------------------------------------------------

/// Run bluetoothctl with the given arguments. Returns Ok(stdout) or Err(stderr).
#[cfg(unix)]
fn run_bluetoothctl(args: &[String]) -> Result<String, String> {
    let output = std::process::Command::new("bluetoothctl")
        .args(args)
        .output()
        .map_err(|e| format!("failed to run bluetoothctl: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}


/// Run `ip` with the given arguments. Returns Ok(stdout) or Err(stderr).
#[cfg(unix)]
fn run_ip(args: &[String]) -> Result<String, String> {
    let output = std::process::Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| format!("failed to run ip: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

// ---------------------------------------------------------------------------
// BtTether manager
// ---------------------------------------------------------------------------

/// Bluetooth tether manager.
pub struct BtTether {
    pub state: BtState,
    pub config: BtConfig,
    pub retry_count: u32,
    pub last_attempt: Option<Instant>,
    pub internet_available: bool,
    pub ip_address: Option<String>,
    /// Dynamic PAN interface name from Network1.Connect (e.g., "bnep0").
    pub pan_interface: Option<String>,
    /// D-Bus connection manager (owns the PAN session lifetime).
    dbus: Option<dbus::DbusBluez>,
    /// Whether the user explicitly disconnected (suppresses auto-reconnect).
    pub user_disconnected: bool,
    /// Receiver for Agent1 pairing events (passkey display/confirmation).
    pub pairing_rx: Option<std::sync::mpsc::Receiver<dbus::PairingEvent>>,
}

fn ip_is_routable(ip: &str) -> bool {
    !ip.starts_with("169.254")
}

impl BtTether {
    /// Create a new Bluetooth tether manager with the given configuration.
    pub fn new(config: BtConfig) -> Self {
        Self {
            state: BtState::Off,
            config,
            retry_count: 0,
            last_attempt: None,
            internet_available: false,
            ip_address: None,
            pan_interface: None,
            dbus: None,
            user_disconnected: false,
            pairing_rx: None,
        }
    }

    /// Whether the D-Bus connection has been initialized.
    pub fn dbus_ready(&self) -> bool {
        self.dbus.is_some()
    }

    /// Ensure D-Bus + Agent1 are initialized (ignores config.enabled).
    /// Used by web pair requests that need D-Bus even when auto-tether is off.
    pub fn ensure_dbus(&mut self) -> Result<(), String> {
        if self.dbus.is_some() {
            return Ok(());
        }
        match dbus::DbusBluez::new() {
            Ok(conn) => {
                self.dbus = Some(conn);
                info!("BT: D-Bus connection established (on-demand)");
            }
            Err(e) => {
                warn!("BT: D-Bus init failed: {e}");
                return Err(e);
            }
        }
        // Register Agent1 for pairing
        if let Some(ref dbus) = self.dbus {
            if let Err(e) = dbus.register_agent() {
                warn!("BT: Agent1 registration failed: {e}");
            }
            let (tx, rx) = std::sync::mpsc::channel();
            if let Err(e) = dbus.setup_agent_handler(tx) {
                warn!("BT: Agent1 crossroads setup failed: {e}");
            } else {
                self.pairing_rx = Some(rx);
            }
        }
        #[cfg(unix)]
        {
            let _ = run_bluetoothctl(&["power".into(), "on".into()]);
        }
        if self.state == BtState::Off {
            self.state = BtState::Disconnected;
        }
        Ok(())
    }

    /// Initialize D-Bus connection and attempt PAN tethering.
    pub fn setup(&mut self) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }

        self.ensure_dbus()?;

        // Try to connect to a paired device
        if self.config.auto_connect {
            // If no paired devices, make discoverable so phone can find us
            let has_paired = self.dbus.as_ref()
                .and_then(|d| d.list_paired_devices().ok())
                .map_or(false, |devs| !devs.is_empty());
            if !has_paired {
                info!("BT: no paired devices — making discoverable for phone pairing");
                self.show();
            }
            let _ = self.connect();
        }

        Ok(())
    }

    /// Attempt PAN connection via D-Bus Network1.Connect("nap").
    pub fn connect(&mut self) -> Result<(), String> {
        self.last_attempt = Some(Instant::now());
        self.state = BtState::Connecting;

        // Check if a bnep interface already exists (from prior session/script)
        if let Some(iface) = Self::find_existing_bnep() {
            info!("BT: adopting existing PAN interface {iface}");
            self.pan_interface = Some(iface.clone());
            self.state = BtState::Connected;
            self.retry_count = 0;
            self.user_disconnected = false;
            self.refresh_ip();
            if let Some(ref ip) = self.ip_address {
                if ip_is_routable(ip) {
                    self.internet_available = true;
                }
            }
            if self.config.hide_after_connect {
                self.hide();
            }
            return Ok(());
        }

        // Re-initialize D-Bus if it was dropped (bus death recovery)
        if self.dbus.is_none() {
            match dbus::DbusBluez::new() {
                Ok(conn) => {
                    info!("BT: D-Bus connection re-established");
                    self.dbus = Some(conn);
                }
                Err(e) => {
                    self.on_error();
                    return Err(format!("D-Bus re-init failed: {e}"));
                }
            }
        }

        // List paired devices (release borrow before find_best_device)
        let devices = match &self.dbus {
            Some(d) => d.list_paired_devices().unwrap_or_default(),
            None => {
                self.on_error();
                return Err("D-Bus not initialized".into());
            }
        };

        let target = self.find_best_device(&devices);
        let device = match target {
            Some(d) => d.clone(),
            None => {
                self.state = BtState::Disconnected;
                return Err("No paired+trusted devices found".into());
            }
        };

        info!("BT: connecting to {} ({})", device.name, device.mac);

        // Wait for NAP UUID before ConnectProfile (like bt-tether plugin)
        if !self.wait_for_nap_uuid(&device.path) {
            warn!("BT: NAP UUID not found on {}", device.name);
            self.on_error();
            return Err("NAP profile not available on device".into());
        }

        self.do_connect_profile(&device.path)
    }

    /// Find the best device to connect to.
    fn find_best_device<'a>(
        &self,
        devices: &'a [dbus::BlueZDevice],
    ) -> Option<&'a dbus::BlueZDevice> {
        if devices.is_empty() {
            return None;
        }
        if !self.config.phone_name.is_empty() {
            let name_lower = self.config.phone_name.to_lowercase();
            if let Some(d) = devices
                .iter()
                .find(|d| d.name.to_lowercase().contains(&name_lower))
            {
                return Some(d);
            }
        }
        if let Some(d) = devices.iter().find(|d| d.connected) {
            return Some(d);
        }
        devices.first()
    }

    /// Disconnect PAN via Network1.Disconnect.
    pub fn disconnect(&mut self) {
        if let Some(ref mut dbus) = self.dbus {
            let _ = dbus.disconnect_pan();
        }
        if let Some(iface) = self.pan_interface.take() {
            let _ = self.release_dhcp(&iface);
        }
        self.ip_address = None;
        self.internet_available = false;
        self.state = BtState::Disconnected;
    }

    /// Check if we should attempt a reconnection.
    pub fn should_connect(&self) -> bool {
        if !self.config.enabled || !self.config.auto_connect || self.user_disconnected {
            return false;
        }
        match self.state {
            BtState::Off | BtState::Pairing | BtState::Connecting | BtState::Connected => false,
            BtState::Disconnected => true,
            BtState::Error => {
                let backoff_secs = BACKOFF_SCHEDULE
                    .get(self.retry_count.saturating_sub(1) as usize)
                    .copied()
                    .unwrap_or(BACKOFF_CAP);
                match self.last_attempt {
                    Some(t) => t.elapsed().as_secs() >= backoff_secs,
                    None => true,
                }
            }
        }
    }

    /// Refresh the IP address from the PAN interface.
    pub fn refresh_ip(&mut self) {
        let iface = match &self.pan_interface {
            Some(i) => i.clone(),
            None => {
                self.ip_address = None;
                return;
            }
        };
        #[cfg(unix)]
        {
            let args = vec!["-4".into(), "addr".into(), "show".into(), iface];
            match run_ip(&args) {
                Ok(output) => {
                    self.ip_address = parse_ip_from_output(&output);
                }
                Err(_) => {
                    self.ip_address = None;
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = iface;
            self.ip_address = None;
        }
    }

    /// Run DHCP on a PAN interface.
    fn run_dhcp(&self, iface: &str) {
        #[cfg(unix)]
        {
            let result = std::process::Command::new("dhcpcd")
                .args(["-n", iface])
                .output();
            match result {
                Ok(o) if o.status.success() => {
                    info!("BT: DHCP via dhcpcd on {iface}");
                    return;
                }
                _ => {}
            }
            let result = std::process::Command::new("dhclient")
                .arg(iface)
                .output();
            match result {
                Ok(o) if o.status.success() => info!("BT: DHCP via dhclient on {iface}"),
                Ok(o) => warn!(
                    "BT: dhclient failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => warn!("BT: DHCP failed: {e}"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = iface;
        }
    }

    /// Release DHCP lease on a PAN interface.
    fn release_dhcp(&self, iface: &str) -> Result<(), String> {
        #[cfg(unix)]
        {
            let _ = std::process::Command::new("dhcpcd")
                .args(["-k", iface])
                .output();
        }
        let _ = iface;
        Ok(())
    }

    /// Check connection status by probing the PAN interface and D-Bus bus.
    pub fn check_status(&mut self) -> BtState {
        // Check if D-Bus bus is still alive
        if let Some(ref dbus) = self.dbus {
            if !dbus.is_bus_alive() {
                warn!("BT: D-Bus connection lost, forcing re-init");
                self.dbus = None;
                self.pan_interface = None;
                self.ip_address = None;
                self.internet_available = false;
                self.state = BtState::Disconnected;
                return self.state;
            }
        }

        // Guard: Connected state requires a known interface.
        // If pan_interface was cleared externally, fall back to Disconnected
        // so should_connect() triggers a reconnect attempt.
        if self.state == BtState::Connected && self.pan_interface.is_none() {
            self.state = BtState::Disconnected;
            self.internet_available = false;
            return self.state;
        }

        if let Some(ref iface) = self.pan_interface {
            let _sysfs = format!("/sys/class/net/{iface}");
            #[cfg(unix)]
            {
                if std::path::Path::new(&_sysfs).exists() {
                    if self.state != BtState::Connected {
                        self.state = BtState::Connected;
                    }
                    return self.state;
                }
                // Interface gone — use backoff to avoid hammering the combo chip
                warn!("BT: PAN interface {iface} disappeared (carrier lost)");
                self.pan_interface = None;
                self.ip_address = None;
                self.internet_available = false;
                self.on_error();
            }
        }
        self.state
    }

    /// Pair, trust, and connect a device via D-Bus.
    pub fn pair_and_connect(&mut self, device_path: &str) -> Result<(), String> {
        self.state = BtState::Pairing;

        let dbus = self.dbus.as_ref().ok_or("D-Bus not initialized")?;

        info!("BT: pairing {device_path}");
        match dbus.pair_device(device_path) {
            Ok(()) => {}
            Err(e) if e.contains("Already Exists") => {
                info!("BT: device already paired, skipping to trust+connect");
            }
            Err(e) => return Err(e),
        }
        dbus.trust_device(device_path)?;
        info!("BT: device trusted");

        // Flush BlueZ pairing data to disk before connecting
        #[cfg(unix)]
        {
            let _ = std::process::Command::new("systemctl")
                .args(&["stop", "bluetooth"])
                .status();
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = std::process::Command::new("systemctl")
                .args(&["start", "bluetooth"])
                .status();
            std::thread::sleep(std::time::Duration::from_secs(2));
            info!("BT: pairing data flushed to disk");
        }

        self.connect_after_pair(device_path)
    }

    /// Connect PAN after pair+trust completed. Waits for NAP UUID,
    /// then calls ConnectProfile with retry on PhoneBusy.
    pub fn connect_after_pair(&mut self, device_path: &str) -> Result<(), String> {
        self.state = BtState::Connecting;

        // Wait for NAP UUID to appear (phone needs time to expose services)
        if !self.wait_for_nap_uuid(device_path) {
            warn!("BT: NAP UUID not found after pairing");
            self.state = BtState::Disconnected;
            return Err("NAP profile not available on device".into());
        }

        self.do_connect_profile(device_path)
    }

    /// Internal: call Network1.Connect with retry logic for PhoneBusy errors.
    fn do_connect_profile(&mut self, device_path: &str) -> Result<(), String> {
        self.state = BtState::Connecting;

        for attempt in 0..BUSY_RETRIES {
            let dbus = self.dbus.as_mut().ok_or("D-Bus not initialized")?;
            match dbus.connect_pan(device_path) {
                Ok(pan) => {
                    info!("BT: PAN connected on {}", pan.interface);
                    self.pan_interface = Some(pan.interface.clone());
                    self.state = BtState::Connected;
                    self.retry_count = 0;
                    self.user_disconnected = false;
                    self.run_dhcp(&pan.interface);
                    self.refresh_ip();
                    if let Some(ref ip) = self.ip_address {
                        if ip_is_routable(ip) {
                            self.internet_available = true;
                        }
                    }
                    if self.config.hide_after_connect {
                        self.hide();
                    }
                    return Ok(());
                }
                Err(dbus::PanConnectError::PhoneBusy) => {
                    // Check if bnep is already up (connection exists from outside)
                    if let Some(iface) = Self::find_existing_bnep() {
                        info!("BT: adopting existing PAN interface {iface}");
                        self.pan_interface = Some(iface.clone());
                        self.state = BtState::Connected;
                        self.retry_count = 0;
                        self.user_disconnected = false;
                        self.refresh_ip();
                        if let Some(ref ip) = self.ip_address {
                            if ip_is_routable(ip) {
                                self.internet_available = true;
                            }
                        }
                        if self.config.hide_after_connect {
                            self.hide();
                        }
                        return Ok(());
                    }
                    if attempt + 1 < BUSY_RETRIES {
                        info!("BT: connection busy, retry {}/{BUSY_RETRIES}...", attempt + 1);
                        std::thread::sleep(std::time::Duration::from_secs(BUSY_RETRY_DELAY_SECS));
                        continue;
                    }
                }
                Err(dbus::PanConnectError::PhoneUnpaired) => {
                    warn!("BT: phone unpaired us — removing stale bond");
                    if let Some(ref d) = self.dbus {
                        let _ = d.remove_device(device_path);
                    }
                    self.on_error();
                    return Err("Phone unpaired Pi — removed stale bond, will re-pair".into());
                }
                Err(e) => {
                    warn!("BT: Network1.Connect failed: {e}");
                    self.on_error();
                    return Err(e.to_string());
                }
            }
        }
        self.on_error();
        Err("Network1.Connect: max retries exhausted".into())
    }

    /// Check if a bnep interface already exists (e.g. from a previous session).
    pub fn find_existing_bnep() -> Option<String> {
        #[cfg(unix)]
        {
            let net_dir = std::path::Path::new("/sys/class/net");
            if let Ok(entries) = std::fs::read_dir(net_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("bnep") || name.starts_with("bt-pan") {
                        return Some(name);
                    }
                }
            }
        }
        None
    }

    /// Poll for NAP UUID on the device (up to NAP_WAIT_SECS).
    /// Returns true if NAP is available.
    fn wait_for_nap_uuid(&self, device_path: &str) -> bool {
        let dbus = match &self.dbus {
            Some(d) => d,
            None => return false,
        };
        for i in 0..NAP_WAIT_SECS {
            if dbus.has_nap_uuid(device_path) {
                if i > 0 {
                    info!("BT: NAP UUID found after {i}s");
                }
                return true;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        false
    }

    /// Remove a device from BlueZ (untrust + remove).
    pub fn forget_device(&mut self, device_path: &str) -> Result<(), String> {
        if let Some(ref dbus) = self.dbus {
            dbus.remove_device(device_path)?;
            info!("BT: removed device {device_path}");
        }
        Ok(())
    }

    /// Scan for nearby BT devices (blocking, ~10s). Returns list of (MAC, name).
    pub fn scan_devices(&self) -> Vec<(String, String)> {
        Self::scan_devices_static()
    }

    /// Static scan — can be called from a background thread without &self.
    /// Uses D-Bus on Linux (creates a temporary connection), falls back to bluetoothctl.
    pub fn scan_devices_static() -> Vec<(String, String)> {
        #[cfg(target_os = "linux")]
        {
            log::info!("BT: scanning for devices via D-Bus (10s)...");
            match dbus::DbusBluez::new() {
                Ok(scan_dbus) => {
                    if let Err(e) = scan_dbus.start_scan() {
                        log::warn!("BT D-Bus scan start failed: {e}, falling back to bluetoothctl");
                        return Self::scan_devices_bluetoothctl();
                    }
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    let _ = scan_dbus.stop_scan();
                    match scan_dbus.list_all_devices() {
                        Ok(devices) => {
                            let results: Vec<(String, String)> = devices
                                .into_iter()
                                .filter(|d| !d.name.is_empty() && d.name != d.mac && !is_mac_like(&d.name))
                                .map(|d| (d.mac, d.name))
                                .collect();
                            log::info!("BT D-Bus scan found {} named devices", results.len());
                            return results;
                        }
                        Err(e) => {
                            log::warn!("BT D-Bus list devices failed: {e}, falling back to bluetoothctl");
                            return Self::scan_devices_bluetoothctl();
                        }
                    }
                }
                Err(e) => {
                    log::warn!("BT D-Bus connection failed for scan: {e}, falling back to bluetoothctl");
                    return Self::scan_devices_bluetoothctl();
                }
            }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            return Self::scan_devices_bluetoothctl();
        }
        #[cfg(not(unix))]
        Vec::new()
    }

    /// Fallback scan via bluetoothctl CLI.
    fn scan_devices_bluetoothctl() -> Vec<(String, String)> {
        #[cfg(unix)]
        {
            let _ = run_bluetoothctl(&build_power_on_args());
            let _ = run_bluetoothctl(&build_agent_on_args());
            match run_bluetoothctl(&build_scan_on_args()) {
                Ok(output) => parse_scan_all_devices(&output),
                Err(e) => {
                    log::error!("BT bluetoothctl scan failed: {e}");
                    Vec::new()
                }
            }
        }
        #[cfg(not(unix))]
        Vec::new()
    }

    /// Process pending D-Bus messages (dispatches Agent1 crossroads handlers).
    pub fn process_dbus(&self) {
        if let Some(ref dbus) = self.dbus {
            dbus.process_messages(std::time::Duration::from_millis(0));
        }
    }

    /// Make BT adapter discoverable (visible to other devices).
    pub fn show(&mut self) {
        #[cfg(unix)]
        {
            // Disable discoverable timeout so it stays visible until explicitly hidden
            let _ = run_bluetoothctl(&["discoverable-timeout".into(), "0".into()]);
            let _ = run_bluetoothctl(&build_discoverable_on_args());
            // Also enable pairable so phones can initiate connections
            let _ = run_bluetoothctl(&["pairable".into(), "on".into()]);
        }
        info!("BT discoverable + pairable ON");
    }

    /// Hide BT adapter (turn off discoverability).
    pub fn hide(&mut self) {
        #[cfg(unix)]
        {
            let _ = run_bluetoothctl(&build_discoverable_off_args());
        }
        info!("BT discoverable OFF");
    }

    /// Power off the BT adapter to free the radio for WiFi monitor mode.
    pub fn power_off(&mut self) {
        self.disconnect();
        #[cfg(unix)]
        {
            let _ = run_bluetoothctl(&build_power_off_args());
        }
        self.state = BtState::Off;
    }

    /// Reconnect tether if configured and not already connected.
    /// Delegates to `connect()` to reuse its error handling and retry state.
    /// No-op if disabled, no paired device, or already connected.
    pub fn ensure_connected(&mut self) {
        if !self.config.enabled {
            return;
        }
        if self.pan_interface.is_some() {
            return;
        }
        if self.user_disconnected {
            return;
        }
        let _ = self.connect();
        self.check_status();
    }

    /// Handle a connection failure.
    pub fn on_error(&mut self) {
        self.state = BtState::Error;
        self.retry_count += 1;
        self.internet_available = false;
        self.ip_address = None;
    }

    /// Get the current IP address, if connected.
    pub fn get_ip(&self) -> Option<&str> {
        self.ip_address.as_deref()
    }

    /// Status string for display (full form).
    pub fn status_str(&self) -> &'static str {
        match self.state {
            BtState::Off => "BT OFF",
            BtState::Disconnected => "BT DISC",
            BtState::Pairing => "BT PAIR",
            BtState::Connecting => "BT ...",
            BtState::Connected => "BT OK",
            BtState::Error => "BT ERR",
        }
    }

    /// Short status for the top bar (matches Python "BT C" / "BT -" format).
    pub fn status_short(&self) -> &'static str {
        match self.state {
            BtState::Off => "-",
            BtState::Disconnected => "-",
            BtState::Pairing => "P",
            BtState::Connecting => ".",
            BtState::Connected => "C",
            BtState::Error => "!",
        }
    }

    /// Toggle connection on/off (called from button handler).
    pub fn toggle(&mut self) {
        match self.state {
            BtState::Connected => self.disconnect(),
            BtState::Off | BtState::Disconnected | BtState::Error => {
                let _ = self.connect();
            }
            BtState::Pairing | BtState::Connecting => {} // ignore during pairing/connection
        }
    }
}

impl Default for BtTether {
    fn default() -> Self {
        Self::new(BtConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== IP parsing tests =====

    #[test]
    fn test_parse_ip_typical_output() {
        let output = r#"4: bnep0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500
    inet 192.168.44.128/24 brd 192.168.44.255 scope global dynamic bnep0
       valid_lft 3599sec preferred_lft 3599sec"#;
        assert_eq!(
            parse_ip_from_output(output),
            Some("192.168.44.128".to_string())
        );
    }

    #[test]
    fn test_parse_ip_no_inet_line() {
        let output = "4: bnep0: <BROADCAST,MULTICAST> mtu 1500\n    link/ether ...\n";
        assert_eq!(parse_ip_from_output(output), None);
    }

    #[test]
    fn test_parse_ip_empty_output() {
        assert_eq!(parse_ip_from_output(""), None);
    }

    #[test]
    fn test_parse_ip_multiple_interfaces() {
        let output = "    inet 10.0.0.1/8 brd 10.255.255.255 scope global\n    inet 192.168.1.100/24 scope global\n";
        assert_eq!(parse_ip_from_output(output), Some("10.0.0.1".to_string()));
    }

    #[test]
    fn test_parse_ip_with_cidr_stripped() {
        let output = "    inet 172.16.0.5/16 brd 172.16.255.255\n";
        let ip = parse_ip_from_output(output);
        assert_eq!(ip, Some("172.16.0.5".to_string()));
    }

    // ===== State machine tests =====

    #[test]
    fn test_default_state() {
        let bt = BtTether::default();
        assert_eq!(bt.state, BtState::Off);
        assert!(!bt.internet_available);
        assert!(bt.ip_address.is_none());
        assert!(bt.pan_interface.is_none());
        assert!(bt.dbus.is_none());
        assert!(!bt.user_disconnected);
    }

    #[test]
    fn test_disconnect() {
        let config = BtConfig {
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Connected;
        bt.internet_available = true;
        bt.ip_address = Some("192.168.44.1".into());
        bt.pan_interface = Some("bnep0".into());
        bt.disconnect();
        assert_eq!(bt.state, BtState::Disconnected);
        assert!(!bt.internet_available);
        assert!(bt.ip_address.is_none());
        assert!(bt.pan_interface.is_none());
    }

    #[test]
    fn test_should_connect_auto_off() {
        // Default config has enabled: false, so should_connect returns false
        let bt = BtTether::default();
        assert!(!bt.should_connect());
    }

    #[test]
    fn test_should_connect_disconnected() {
        let config = BtConfig {
            enabled: true,
            auto_connect: true,
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Disconnected;
        assert!(bt.should_connect());
    }

    #[test]
    fn test_should_connect_already_connected() {
        let config = BtConfig {
            enabled: true,
            auto_connect: true,
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Connected;
        assert!(!bt.should_connect());
    }

    #[test]
    fn test_should_connect_while_connecting() {
        let config = BtConfig {
            enabled: true,
            auto_connect: true,
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Connecting;
        assert!(!bt.should_connect());
    }

    #[test]
    fn test_error_retry_interval_elapsed() {
        let config = BtConfig {
            enabled: true,
            auto_connect: true,
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Error;
        bt.retry_count = 2;
        // With retry_count=2, backoff = BACKOFF_SCHEDULE[1] = 60s
        bt.last_attempt = Some(Instant::now() - std::time::Duration::from_secs(61));
        assert!(bt.should_connect());
    }

    #[test]
    fn test_error_retry_interval_not_elapsed() {
        let config = BtConfig {
            enabled: true,
            auto_connect: true,
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Error;
        bt.retry_count = 1;
        bt.last_attempt = Some(Instant::now());
        assert!(!bt.should_connect());
    }

    #[test]
    fn test_on_error() {
        let mut bt = BtTether::default();
        bt.state = BtState::Connecting;
        bt.internet_available = true;
        bt.ip_address = Some("1.2.3.4".into());
        bt.on_error();
        assert_eq!(bt.state, BtState::Error);
        assert_eq!(bt.retry_count, 1);
        assert!(!bt.internet_available);
        assert!(bt.ip_address.is_none());
    }

    #[test]
    fn test_on_error_increments() {
        let mut bt = BtTether::default();
        bt.on_error();
        bt.on_error();
        bt.on_error();
        assert_eq!(bt.retry_count, 3);
    }

    #[test]
    fn test_status_strings() {
        let mut bt = BtTether::default();
        assert_eq!(bt.status_str(), "BT OFF");
        bt.state = BtState::Connected;
        assert_eq!(bt.status_str(), "BT OK");
        bt.state = BtState::Error;
        assert_eq!(bt.status_str(), "BT ERR");
        bt.state = BtState::Disconnected;
        assert_eq!(bt.status_str(), "BT DISC");
        bt.state = BtState::Connecting;
        assert_eq!(bt.status_str(), "BT ...");
    }

    #[test]
    fn test_status_short() {
        let mut bt = BtTether::default();
        assert_eq!(bt.status_short(), "-");
        bt.state = BtState::Connected;
        assert_eq!(bt.status_short(), "C");
        bt.state = BtState::Connecting;
        assert_eq!(bt.status_short(), ".");
        bt.state = BtState::Error;
        assert_eq!(bt.status_short(), "!");
        bt.state = BtState::Disconnected;
        assert_eq!(bt.status_short(), "-");
    }

    // ===== Toggle state machine tests =====

    #[test]
    fn test_toggle_ignored_while_connecting() {
        let config = BtConfig {
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Connecting;
        bt.toggle();
        assert_eq!(bt.state, BtState::Connecting);
    }

    // ===== Status detection tests =====

    #[test]
    fn test_check_status_from_disconnected() {
        let mut bt = BtTether::default();
        bt.state = BtState::Disconnected;
        let state = bt.check_status();
        assert_eq!(state, BtState::Disconnected);
    }

    // ===== Config default tests =====

    #[test]
    fn test_config_defaults() {
        let cfg = BtConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.phone_name.is_empty());
        assert!(cfg.auto_connect);
        assert!(cfg.hide_after_connect);
    }

    // ===== IP getter tests =====

    #[test]
    fn test_get_ip_none() {
        let bt = BtTether::default();
        assert!(bt.get_ip().is_none());
    }

    #[test]
    fn test_get_ip_some() {
        let mut bt = BtTether::default();
        bt.ip_address = Some("192.168.44.128".into());
        assert_eq!(bt.get_ip(), Some("192.168.44.128"));
    }

    // ===== Bluetoothctl builder tests =====

    #[test]
    fn test_build_power_on_args() {
        assert_eq!(build_power_on_args(), vec!["power", "on"]);
    }

    #[test]
    fn test_build_agent_on_args() {
        assert_eq!(build_agent_on_args(), vec!["agent", "on"]);
    }

    #[test]
    fn test_build_default_agent_args() {
        assert_eq!(build_default_agent_args(), vec!["default-agent"]);
    }

    #[test]
    fn test_build_pair_args() {
        let args = build_pair_args("AA:BB:CC:DD:EE:FF");
        assert_eq!(args, vec!["pair", "AA:BB:CC:DD:EE:FF"]);
    }

    #[test]
    fn test_build_trust_args() {
        let args = build_trust_args("AA:BB:CC:DD:EE:FF");
        assert_eq!(args, vec!["trust", "AA:BB:CC:DD:EE:FF"]);
    }

    #[test]
    fn test_build_discoverable_off_args() {
        assert_eq!(build_discoverable_off_args(), vec!["discoverable", "off"]);
    }

    #[test]
    fn test_build_discoverable_on_args() {
        assert_eq!(build_discoverable_on_args(), vec!["discoverable", "on"]);
    }

    #[test]
    fn test_build_scan_on_args() {
        assert_eq!(build_scan_on_args(), vec!["--timeout", "10", "scan", "on"]);
    }

    // ===== Scan output parser tests =====

    #[test]
    fn test_parse_scan_all_devices_basic() {
        let output =
            "[NEW] Device AA:BB:CC:DD:EE:FF My Phone\n[NEW] Device 11:22:33:44:55:66 Galaxy S24";
        let devices = parse_scan_all_devices(output);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0], ("AA:BB:CC:DD:EE:FF".into(), "My Phone".into()));
        assert_eq!(
            devices[1],
            ("11:22:33:44:55:66".into(), "Galaxy S24".into())
        );
    }

    #[test]
    fn test_parse_scan_all_devices_skips_mac_names() {
        let output = "[NEW] Device AA:BB:CC:DD:EE:FF AA-BB-CC-DD-EE-FF\n[NEW] Device 11:22:33:44:55:66 Real Phone";
        let devices = parse_scan_all_devices(output);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].1, "Real Phone");
    }

    #[test]
    fn test_parse_scan_all_devices_allows_hyphenated_names() {
        let output = "[NEW] Device AA:BB:CC:DD:EE:FF Galaxy S24-Ultra\n[NEW] Device 11:22:33:44:55:66 Mi-Band";
        let devices = parse_scan_all_devices(output);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].1, "Galaxy S24-Ultra");
        assert_eq!(devices[1].1, "Mi-Band");
    }

    #[test]
    fn test_parse_scan_all_devices_empty() {
        assert!(parse_scan_all_devices("").is_empty());
        assert!(parse_scan_all_devices("some random log output").is_empty());
    }

    #[test]
    fn test_parse_scan_all_devices_ansi() {
        let output = "\x1b[1;34m[NEW] Device AA:BB:CC:DD:EE:FF My Phone\x1b[0m";
        let devices = parse_scan_all_devices(output);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].1, "My Phone");
    }

    #[test]
    fn test_is_mac_like() {
        assert!(is_mac_like("AA-BB-CC-DD-EE-FF"));
        assert!(is_mac_like("aa:bb:cc:dd:ee:ff"));
        assert!(!is_mac_like("Galaxy S24-Ultra"));
        assert!(!is_mac_like("Mi-Band"));
        assert!(!is_mac_like("short"));
        assert!(!is_mac_like(""));
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\x1b[0;92m[NEW]\x1b[0m Device"), "[NEW] Device");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
        assert_eq!(strip_ansi(""), "");
    }

    // ===== Pairing state tests =====

    #[test]
    fn test_pairing_state() {
        let mut bt = BtTether::default();
        bt.state = BtState::Pairing;
        assert_eq!(bt.status_str(), "BT PAIR");
        assert_eq!(bt.status_short(), "P");
    }

    #[test]
    fn test_should_connect_pairing() {
        let config = BtConfig {
            enabled: true,
            auto_connect: true,
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Pairing;
        assert!(!bt.should_connect());
    }

    #[test]
    fn test_toggle_pairing_noop() {
        let config = BtConfig {
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Pairing;
        bt.toggle();
        assert_eq!(bt.state, BtState::Pairing);
    }

    #[test]
    fn test_setup_not_enabled() {
        let config = BtConfig {
            enabled: false,
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        let result = bt.setup();
        assert!(result.is_ok());
        assert_eq!(bt.state, BtState::Off);
    }

    // ===== Backoff schedule tests =====

    #[test]
    fn test_backoff_schedule() {
        assert_eq!(BACKOFF_SCHEDULE, &[30, 60, 120, 300]);
        assert_eq!(BACKOFF_CAP, 300);
    }

    // ===== find_best_device tests =====

    #[test]
    fn test_find_best_device_empty() {
        let bt = BtTether::default();
        let devices: Vec<dbus::BlueZDevice> = vec![];
        assert!(bt.find_best_device(&devices).is_none());
    }

    #[test]
    fn test_find_best_device_by_name() {
        let config = BtConfig {
            phone_name: "iPhone".into(),
            ..Default::default()
        };
        let bt = BtTether::new(config);
        let devices = vec![
            dbus::BlueZDevice {
                path: "/org/bluez/hci0/dev_AA".into(),
                mac: "AA:BB:CC:DD:EE:FF".into(),
                name: "Galaxy".into(),
                paired: true,
                trusted: true,
                connected: false,
            },
            dbus::BlueZDevice {
                path: "/org/bluez/hci0/dev_BB".into(),
                mac: "11:22:33:44:55:66".into(),
                name: "iPhone 15".into(),
                paired: true,
                trusted: true,
                connected: false,
            },
        ];
        let best = bt.find_best_device(&devices).unwrap();
        assert_eq!(best.name, "iPhone 15");
    }

    #[test]
    fn test_find_best_device_prefers_connected() {
        let bt = BtTether::default();
        let devices = vec![
            dbus::BlueZDevice {
                path: "/org/bluez/hci0/dev_AA".into(),
                mac: "AA:BB:CC:DD:EE:FF".into(),
                name: "Phone A".into(),
                paired: true,
                trusted: true,
                connected: false,
            },
            dbus::BlueZDevice {
                path: "/org/bluez/hci0/dev_BB".into(),
                mac: "11:22:33:44:55:66".into(),
                name: "Phone B".into(),
                paired: true,
                trusted: true,
                connected: true,
            },
        ];
        let best = bt.find_best_device(&devices).unwrap();
        assert_eq!(best.name, "Phone B");
    }

    #[test]
    fn test_user_disconnected_prevents_reconnect() {
        let config = BtConfig {
            enabled: true,
            auto_connect: true,
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Disconnected;
        bt.user_disconnected = true;
        assert!(!bt.should_connect());
    }

    #[test]
    fn test_should_connect_disabled() {
        let config = BtConfig {
            enabled: false,
            auto_connect: true,
            ..Default::default()
        };
        let mut bt = BtTether::new(config);
        bt.state = BtState::Disconnected;
        assert!(!bt.should_connect());
    }

    #[test]
    fn test_ensure_connected_no_op_when_disabled() {
        let mut bt = BtTether::new(BtConfig {
            enabled: false,
            phone_name: "iPhone".into(),
            ..Default::default()
        });
        bt.state = BtState::Disconnected;
        bt.ensure_connected(); // must not panic or change state
        assert_eq!(bt.state, BtState::Disconnected);
    }

    #[test]
    fn test_ensure_connected_no_op_when_already_connected() {
        let mut bt = BtTether::new(BtConfig {
            enabled: true,
            phone_name: "iPhone".into(),
            ..Default::default()
        });
        bt.pan_interface = Some("bnep0".into());
        bt.ensure_connected();
        // Should be no-op since already connected
    }

    #[test]
    fn test_ip_is_routable_returns_true_for_normal_ip() {
        assert!(ip_is_routable("192.168.44.128"), "normal IP should be routable");
        assert!(ip_is_routable("10.0.0.1"), "10.x should be routable");
        assert!(ip_is_routable("172.16.0.1"), "172.16.x should be routable");
    }

    #[test]
    fn test_ip_is_routable_returns_false_for_apipa() {
        assert!(!ip_is_routable("169.254.0.1"), "APIPA should not be routable");
        assert!(!ip_is_routable("169.254.255.255"), "APIPA max should not be routable");
    }

    #[test]
    fn test_check_status_clears_zombie_connected_state() {
        let mut bt = BtTether::default();
        bt.state = BtState::Connected;
        bt.pan_interface = None;       // zombie: Connected but no interface known
        bt.internet_available = true;
        let state = bt.check_status();
        assert_eq!(state, BtState::Disconnected, "zombie Connected+no-iface should become Disconnected");
        assert!(!bt.internet_available, "internet_available must clear with zombie state");
    }
}

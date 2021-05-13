//! Bluetooth adapter.

use dbus::{
    arg::{RefArg, Variant},
    nonblock::{Proxy, SyncConnection},
    Path,
};
use futures::{
    channel::{mpsc, oneshot},
    SinkExt, Stream, StreamExt,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::{Debug, Formatter},
    sync::Arc,
    u32,
};
use strum::{Display, EnumString};
use uuid::Uuid;

use crate::{
    advertising, all_dbus_objects, device, device::Device, gatt, Address, AddressType, Error, LeAdvertisement,
    LeAdvertisementFeature, LeAdvertisementHandle, LeAdvertisementSecondaryChannel, LeAdvertisingCapabilities,
    LeAdvertisingFeature, Modalias, ObjectEvent, PropertyEvent, Result, SessionInner, SERVICE_NAME, TIMEOUT,
};

pub(crate) const INTERFACE: &str = "org.bluez.Adapter1";
pub(crate) const PREFIX: &str = "/org/bluez/";

/// Interface to a Bluetooth adapter.
#[derive(Clone)]
pub struct Adapter {
    inner: Arc<SessionInner>,
    dbus_path: Path<'static>,
    name: Arc<String>,
}

impl Debug for Adapter {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "Adapter {{ name: {} }}", self.name())
    }
}

/// Bluetooth device event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceEvent {
    /// Device added.
    Added(Address),
    /// Device removed.
    Removed(Address),
}

impl Adapter {
    /// Create Bluetooth adapter interface for adapter with specified name.
    pub(crate) fn new(inner: Arc<SessionInner>, name: &str) -> Result<Self> {
        Ok(Self {
            inner,
            dbus_path: Path::new(PREFIX.to_string() + name).map_err(|_| Error::InvalidName(name.to_string()))?,
            name: Arc::new(name.to_string()),
        })
    }

    fn proxy(&self) -> Proxy<'_, &SyncConnection> {
        Proxy::new(SERVICE_NAME, &self.dbus_path, TIMEOUT, &*self.inner.connection)
    }

    pub(crate) fn dbus_path(adapter_name: &str) -> Result<Path<'static>> {
        Path::new(format!("{}{}", PREFIX, adapter_name,))
            .map_err(|_| Error::InvalidName((*adapter_name).to_string()))
    }

    pub(crate) fn parse_dbus_path_prefix<'a>(path: &'a Path) -> Option<(&'a str, &'a str)> {
        match path.strip_prefix(PREFIX) {
            Some(p) => {
                let sep = p.find('/').unwrap_or(p.len());
                Some((&p[0..sep], &p[sep..]))
            }
            None => None,
        }
    }

    pub(crate) fn parse_dbus_path<'a>(path: &'a Path) -> Option<&'a str> {
        match Self::parse_dbus_path_prefix(path) {
            Some((v, "")) => Some(v),
            _ => None,
        }
    }

    /// The Bluetooth adapter name.
    ///
    /// For example `hci0`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Bluetooth addresses of discovered Bluetooth devices.
    pub async fn device_addresses(&self) -> Result<Vec<Address>> {
        let mut addrs = Vec::new();
        for (path, interfaces) in all_dbus_objects(&*self.inner.connection).await? {
            match Device::parse_dbus_path(&path) {
                Some((adapter, addr)) if adapter == *self.name && interfaces.contains_key(device::INTERFACE) => {
                    addrs.push(addr)
                }
                _ => (),
            }
        }
        Ok(addrs)
    }

    /// Get interface to Bluetooth device of specified address.
    pub fn device(&self, address: Address) -> Result<Device> {
        Device::new(self.inner.clone(), self.name.clone(), address)
    }

    /// Stream device added and removed events.
    pub async fn device_changes(&self) -> Result<impl Stream<Item = DeviceEvent>> {
        let adapter_path = self.dbus_path.clone().into_static();
        let obj_events = ObjectEvent::stream(self.inner.connection.clone(), Some(adapter_path.clone())).await?;

        let my_name = self.name.clone();
        let events = obj_events.filter_map(move |evt| {
            let my_name = my_name.clone();
            async move {
                match evt {
                    ObjectEvent::Added { object, .. } => match Device::parse_dbus_path(&object) {
                        Some((adapter, address)) if adapter == *my_name => Some(DeviceEvent::Added(address)),
                        _ => None,
                    },
                    ObjectEvent::Removed { object, .. } => match Device::parse_dbus_path(&object) {
                        Some((adapter, address)) if adapter == *my_name => Some(DeviceEvent::Removed(address)),
                        _ => None,
                    },
                }
            }
        });
        Ok(events)
    }

    /// This method starts the device discovery session.
    ///
    /// This
    /// includes an inquiry procedure and remote device name
    /// resolving.
    ///
    /// This process will start creating Device objects as
    /// new devices are discovered.
    /// During discovery RSSI delta-threshold is imposed.    
    ///
    /// When multiple clients create discovery sessions, their
    /// filters are internally merged, and notifications about
    /// new devices are sent to all clients. Therefore, each
    /// client must check that device updates actually match
    /// its filter.    
    ///
    /// Only one discovery session may be active per Bluetooth adapter.
    /// Use the `device_events` method to get notified when a device is discovered.
    /// Drop the `DeviceDiscovery` to stop the discovery process.
    pub async fn discover_devices(&self, filter: DiscoveryFilter) -> Result<DeviceDiscovery> {
        let mut discovery_slots = self.inner.discovery_slots.lock().await;
        if let Some(mut rx) = discovery_slots.remove(&*self.name) {
            if let Ok(None) = rx.try_recv() {
                discovery_slots.insert((*self.name).clone(), rx);
                return Err(Error::AnotherDiscoveryInProgress);
            }
        }
        let (done_tx, done_rx) = oneshot::channel();
        discovery_slots.insert((*self.name).clone(), done_rx);

        DeviceDiscovery::new(self.inner.clone(), self.dbus_path.clone(), self.name.clone(), filter, done_tx).await
    }

    dbus_interface!();
    dbus_default_interface!(INTERFACE);

    /// Streams adapter property changes.
    pub async fn changes(&self) -> Result<impl Stream<Item = AdapterChanged>> {
        let mut events = PropertyEvent::stream(self.inner.connection.clone(), self.dbus_path.clone()).await?;

        let (mut tx, rx) = mpsc::unbounded();
        let name = self.name.clone();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                for property in AdapterProperty::from_prop_map(event.changed) {
                    if tx.send(AdapterChanged { name: name.clone(), property }).await.is_err() {
                        break;
                    }
                }
            }
        });

        Ok(rx)
    }

    /// Registers an advertisement object to be sent over the LE
    /// Advertising channel.
    ///
    /// InvalidArguments error indicates that the object has
    /// invalid or conflicting properties.
    ///
    /// InvalidLength error indicates that the data
    /// provided generates a data packet which is too long.
    ///
    /// The properties of this object are parsed when it is
    /// registered, and any changes are ignored.
    ///
    /// If the same object is registered twice it will result in
    /// an AlreadyExists error.
    ///
    /// If the maximum number of advertisement instances is
    /// reached it will result in NotPermitted error.    
    ///
    /// Drop the returned `LeAdvertisementHandle` to unregister the advertisement.
    pub async fn le_advertise(&self, le_advertisement: LeAdvertisement) -> Result<LeAdvertisementHandle> {
        le_advertisement.register(self.inner.clone(), self.name.clone()).await
    }

    /// Registers a local GATT services hierarchy (GATT Server).
    ///
    /// Registering a service allows applications to publish a *local* GATT service,
    /// which then becomes available to remote devices.
    ///
    /// Drop the returned `ApplicationHandle` to unregister the application.
    pub async fn serve_gatt_application(
        &self, gatt_application: gatt::local::Application,
    ) -> Result<gatt::local::ApplicationHandle> {
        gatt_application.register(self.inner.clone(), self.name.clone()).await
    }

    /// Registers local GATT profiles (GATT Client).    
    ///
    /// By registering this type of object
    /// an application effectively indicates support for a specific GATT profile
    /// and requests automatic connections to be established to devices
    /// supporting it.
    ///
    /// Drop the returned `ProfileHandle` to unregister the application.
    pub async fn register_gatt_profile(
        &self, gatt_profile: gatt::local::Profile,
    ) -> Result<gatt::local::ProfileHandle> {
        gatt_profile.register(self.inner.clone(), self.name.clone()).await
    }

    // ===========================================================================================
    // Methods
    // ===========================================================================================

    /// This removes the remote device object for the given
    /// device address.
    ///
    /// It will remove also the pairing information.
    pub async fn remove_device(&self, address: Address) -> Result<()> {
        let path = Device::dbus_path(self.name(), address)?;
        self.call_method("RemoveDevice", ((path),)).await?;
        Ok(())
    }

    /// This method connects to device without need of
    /// performing General Discovery.
    ///
    /// Connection mechanism is
    /// similar to Connect method from Device1 interface with
    /// exception that this method returns success when physical
    /// connection is established. After this method returns,
    /// services discovery will continue and any supported
    /// profile will be connected. There is no need for calling
    /// Connect on Device1 after this call. If connection was
    /// successful this method returns the created
    /// device object.
    ///
    /// Parameters that may be set in the filter dictionary
    /// include the following:    
    ///
    ///  `address` -
    ///     The Bluetooth device address of the remote
    ///     device. This parameter is mandatory.
    ///
    /// `address_type` -
    ///     The Bluetooth device Address Type. This is
    ///     address type that should be used for initial
    ///     connection. If this parameter is not present
    ///     BR/EDR device is created.    
    ///
    /// This method is experimental.
    pub async fn connect_device(&self, address: Address, address_type: Option<AddressType>) -> Result<Device> {
        let mut m = HashMap::new();
        m.insert("Address", address.to_string());
        if let Some(address_type) = address_type {
            m.insert("AddressType", address_type.to_string());
        }
        let (_path,): (Path,) = self.call_method("ConnectDevice", (m,)).await?;

        self.device(address)
    }
}

define_properties!(
    Adapter, pub AdapterProperty => {

        // ===========================================================================================
        // Adapter properties
        // ===========================================================================================

        /// The Bluetooth device address.
        property(
            Address, Address,
            dbus: (INTERFACE, "Address", String, MANDATORY),
            get: (address, v => { v.parse()? }),
        );

        /// The Bluetooth Address Type.
        ///
        /// For dual-mode and BR/EDR
        /// only adapter this defaults to "public". Single mode LE
        /// adapters may have either value. With privacy enabled
        /// this contains type of Identity Address and not type of
        /// address used for connection.
        property(
            AddressType, AddressType,
            dbus: (INTERFACE, "AddressType", String, MANDATORY),
            get: (address_type, v => {v.parse()?}),
        );

        ///	The Bluetooth system name (pretty hostname).
        ///
        /// This property is either a static system default
        /// or controlled by an external daemon providing
        /// access to the pretty hostname configuration.
        property(
            SystemName, String,
            dbus: (INTERFACE, "Name", String, MANDATORY),
            get: (system_name, v => {v.to_owned()}),
        );

        /// The Bluetooth friendly name.
        ///
        /// This value can be changed.
        ///
        /// In case no alias is set, it will return the system
        /// provided name. Setting an empty string as alias will
        /// convert it back to the system provided name.
        ///
        /// When resetting the alias with an empty string, the
        /// property will default back to system name.
        ///
        /// On a well configured system, this property never
        /// needs to be changed since it defaults to the system
        /// name and provides the pretty hostname. Only if the
        /// local name needs to be different from the pretty
        /// hostname, this property should be used as last
        /// resort.
        property(
            Alias, String,
            dbus: (INTERFACE, "Alias", String, MANDATORY),
            get: (alias, v => {v.to_owned()}),
            set: (set_alias, v => {v}),
        );

        /// The Bluetooth class of device.
        ///
        ///	This property represents the value that is either
        ///	automatically configured by DMI/ACPI information
        ///	or provided as static configuration.
        property(
            Class, u32,
            dbus: (INTERFACE, "Class", u32, MANDATORY),
            get: (class, v => {v.to_owned()}),
        );

        /// Switch an adapter on or off. This will also set the
        /// appropriate connectable state of the controller.
        ///
        /// The value of this property is not persistent. After
        /// restart or unplugging of the adapter it will reset
        /// back to false.
        property(
            Powered, bool,
            dbus: (INTERFACE, "Powered", bool, MANDATORY),
            get: (is_powered, v => {v.to_owned()}),
            set: (set_powered, v => {v}),
        );

        /// Switch an adapter to discoverable or non-discoverable
        /// to either make it visible or hide it.
        ///
        /// This is a global
        /// setting and should only be used by the settings
        /// application.
        ///
        /// If the DiscoverableTimeout is set to a non-zero
        /// value then the system will set this value back to
        /// false after the timer expired.
        ///
        /// In case the adapter is switched off, setting this
        /// value will fail.
        ///
        /// When changing the Powered property the new state of
        /// this property will be updated via a PropertiesChanged
        /// signal.
        ///
        /// For any new adapter this settings defaults to false.
        property(
            Discoverable, bool,
            dbus: (INTERFACE, "Discoverable", bool, MANDATORY),
            get: (is_discoverable, v => {v.to_owned()}),
            set: (set_discoverable, v => {v}),
        );

        /// Switch an adapter to pairable or non-pairable.
        ///
        /// This is
        /// a global setting and should only be used by the
        /// settings application.
        ///
        /// Note that this property only affects incoming pairing
        /// requests.
        ///
        /// For any new adapter this settings defaults to true.
        property(
            Pairable, bool,
            dbus: (INTERFACE, "Pairable", bool, MANDATORY),
            get: (is_pairable, v => {v.to_owned()}),
            set: (set_pairable, v => {v}),
        );

        /// The pairable timeout in seconds.
        ///
        /// A value of zero
        /// means that the timeout is disabled and it will stay in
        /// pairable mode forever.
        ///
        /// The default value for pairable timeout should be
        /// disabled (value 0).
        property(
            PairableTimeout, u32,
            dbus: (INTERFACE, "PairableTimeout", u32, MANDATORY),
            get: (pairable_timeout, v => {v.to_owned()}),
            set: (set_pairable_timeout, v => {v}),
        );

        /// The discoverable timeout in seconds.
        ///
        /// A value of zero
        /// means that the timeout is disabled and it will stay in
        /// discoverable/limited mode forever.
        ///
        /// The default value for the discoverable timeout should
        /// be 180 seconds (3 minutes).
        property(
            DiscoverableTimeout, u32,
            dbus: (INTERFACE, "DiscoverableTimeout", u32, MANDATORY),
            get: (discoverable_timeout, v => {v.to_owned()}),
            set: (set_discoverable_timeout, v => {v}),
        );

        ///	Indicates that a device discovery procedure is active.
        property(
            Discovering, bool,
            dbus: (INTERFACE, "Discovering", bool, MANDATORY),
            get: (is_discovering, v => {v.to_owned()}),
        );

        /// List of 128-bit UUIDs that represents the available
        /// local services.
        property(
            Uuids, HashSet<Uuid>,
            dbus: (INTERFACE, "UUIDs", Vec<String>, OPTIONAL),
            get: (uuids, v => {
                v
                .into_iter()
                .map(|uuid| {
                    uuid.parse()
                        .map_err(|_| Error::InvalidUuid(uuid.to_string()))
                })
                .collect::<Result<HashSet<Uuid>>>()?
            }),
        );

        /// Local Device ID information in modalias format
        /// used by the kernel and udev.
        property(
            Modalias, Modalias,
            dbus: (INTERFACE, "Modalias", String, OPTIONAL),
            get: (modalias, v => { v.parse()? }),
        );

        // ===========================================================================================
        // LE advertising manager properties
        // ===========================================================================================

        ///	Number of active advertising instances.
        property(
            ActiveAdvertisingInstances, u8,
            dbus: (advertising::MANAGER_INTERFACE, "ActiveInstances", u8, MANDATORY),
            get: (active_advertising_instances, v => {v.to_owned()}),
        );

        ///	Number of available advertising instances.
        property(
            SupportedAdvertisingInstances, u8,
            dbus: (advertising::MANAGER_INTERFACE, "SupportedInstances", u8, MANDATORY),
            get: (supported_advertising_instances, v => {v.to_owned()}),
        );

        /// List of supported system includes.
        property(
            SupportedAdvertisingSystemIncludes, BTreeSet<LeAdvertisementFeature>,
            dbus: (advertising::MANAGER_INTERFACE, "SupportedIncludes", Vec<String>, MANDATORY),
            get: (supported_advertising_system_includes, v => {
                v.iter().filter_map(|s| s.parse().ok()).collect()
            }),
        );

        /// List of supported Secondary channels.
        ///
        /// Secondary
        /// channels can be used to advertise with the
        /// corresponding PHY.
        property(
            SupportedAdvertisingSecondaryChannels, BTreeSet<LeAdvertisementSecondaryChannel>,
            dbus: (advertising::MANAGER_INTERFACE, "SupportedSecondaryChannels", Vec<String>, MANDATORY),
            get: (supported_advertising_secondary_channels, v => {
                v.iter().filter_map(|s| s.parse().ok()).collect()
            }),
        );

        /// Enumerates Advertising-related controller capabilities
        /// useful to the client.
        property(
            SupportedAdvertisingCapabilities, LeAdvertisingCapabilities,
            dbus: (advertising::MANAGER_INTERFACE, "SupportedCapabilities", HashMap<String, Variant<Box<dyn RefArg  + 'static>>>, OPTIONAL),
            get: (supported_advertising_capabilities, v => {
                LeAdvertisingCapabilities::from_dict(v)?
            }),
        );

        /// List of supported platform features.
        ///
        /// If no features
        /// are available on the platform, the SupportedFeatures
        /// array will be empty.
        property(
            SupportedAdvertisingFeatures, BTreeSet<LeAdvertisingFeature>,
            dbus: (advertising::MANAGER_INTERFACE, "SupportedFeatures", Vec<String>, OPTIONAL),
            get: (supported_advertising_features, v => {
                v.iter().filter_map(|s| s.parse().ok()).collect()
            }),
        );
    }
);

/// Bluetooth adapter property change event.
#[derive(Debug, Clone)]
pub struct AdapterChanged {
    /// Name of changed Bluetooth adapter.
    pub name: Arc<String>,
    /// Changed property.
    pub property: AdapterProperty,
}

/// Transport parameter determines the type of scan.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Display, EnumString)]
pub enum DiscoveryTransport {
    /// interleaved scan
    #[strum(serialize = "auto")]
    Auto,
    /// BR/EDR inquiry
    #[strum(serialize = "bredr")]
    BrEdr,
    /// LE scan only
    #[strum(serialize = "le")]
    Le,
}

impl Default for DiscoveryTransport {
    fn default() -> Self {
        Self::Auto
    }
}

/// Bluetooth device discovery filter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryFilter {
    ///  Filter by service UUIDs, empty means match
    ///  _any_ UUID.
    ///
    ///  When a remote device is found that advertises
    ///  any UUID from UUIDs, it will be reported if:
    ///  - Pathloss and RSSI are both empty.
    ///  - only Pathloss param is set, device advertise
    ///    TX pwer, and computed pathloss is less than
    ///    Pathloss param.
    ///  - only RSSI param is set, and received RSSI is
    ///    higher than RSSI param.    
    pub uuids: HashSet<Uuid>,
    /// RSSI threshold value.
    ///
    /// PropertiesChanged signals will be emitted
    /// for already existing Device objects, with
    /// updated RSSI value. If one or more discovery
    /// filters have been set, the RSSI delta-threshold,
    /// that is imposed by StartDiscovery by default,
    /// will not be applied.
    pub rssi: Option<i16>,
    /// Pathloss threshold value.
    ///
    /// PropertiesChanged signals will be emitted
    /// for already existing Device objects, with
    /// updated Pathloss value.
    pub pathloss: Option<u16>,
    /// Transport parameter determines the type of
    /// scan.
    ///
    /// Possible values:
    ///     "auto"	- interleaved scan
    ///     "bredr"	- BR/EDR inquiry
    ///     "le"	- LE scan only
    ///
    /// If "le" or "bredr" Transport is requested,
    /// and the controller doesn't support it,
    /// org.bluez.Error.Failed error will be returned.
    ///
    /// If "auto" transport is requested, scan will use
    /// LE, BREDR, or both, depending on what's
    /// currently enabled on the controller.
    pub transport: DiscoveryTransport,
    /// Disables duplicate detection of advertisement data.
    ///
    /// When enabled PropertiesChanged signals will be
    /// generated for either ManufacturerData and
    /// ServiceData everytime they are discovered.
    pub duplicate_data: bool,
    /// Make adapter discoverable while discovering.
    ///
    /// If the adapter is already discoverable setting
    /// this filter won't do anything.
    pub discoverable: bool,
    /// Discover devices where the pattern matches
    /// either the prefix of the address or
    /// device name which is convenient way to limited
    /// the number of device objects created during a
    /// discovery.
    ///
    ///	When set disregards device discoverable flags.
    ///
    /// Note: The pattern matching is ignored if there
    /// are other client that don't set any pattern as
    /// it work as a logical OR, also setting empty
    /// string "" pattern will match any device found.
    pub pattern: Option<String>,
}

impl Default for DiscoveryFilter {
    fn default() -> Self {
        Self {
            uuids: Default::default(),
            rssi: Default::default(),
            pathloss: Default::default(),
            transport: Default::default(),
            duplicate_data: true,
            discoverable: false,
            pattern: Default::default(),
        }
    }
}

impl DiscoveryFilter {
    fn to_dict(self) -> HashMap<&'static str, Variant<Box<dyn RefArg>>> {
        let mut hm: HashMap<&'static str, Variant<Box<dyn RefArg>>> = HashMap::new();
        let Self { uuids, rssi, pathloss, transport, duplicate_data, discoverable, pattern } = self;
        hm.insert("UUIDs", Variant(Box::new(uuids.into_iter().map(|uuid| uuid.to_string()).collect::<Vec<_>>())));
        if let Some(rssi) = rssi {
            hm.insert("RSSI", Variant(Box::new(rssi)));
        }
        if let Some(pathloss) = pathloss {
            hm.insert("Pathloss", Variant(Box::new(pathloss)));
        }
        hm.insert("Transport", Variant(Box::new(transport.to_string())));
        hm.insert("DuplicateData", Variant(Box::new(duplicate_data)));
        hm.insert("Discoverable", Variant(Box::new(discoverable)));
        if let Some(pattern) = pattern {
            hm.insert("Pattern", Variant(Box::new(pattern)));
        }
        hm
    }
}

/// Device discovery session.
///
/// Drop to stop discovery.
pub struct DeviceDiscovery {
    adapter_name: Arc<String>,
    _term_tx: oneshot::Sender<()>,
}

impl Debug for DeviceDiscovery {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "DeviceDiscovery {{ adapter_name: {} }}", self.adapter_name)
    }
}

impl DeviceDiscovery {
    pub(crate) async fn new<'a>(
        inner: Arc<SessionInner>, dbus_path: Path<'static>, adapter_name: Arc<String>, filter: DiscoveryFilter,
        done_tx: oneshot::Sender<()>,
    ) -> Result<Self> {
        let proxy = Proxy::new(SERVICE_NAME, &dbus_path, TIMEOUT, &*inner.connection);
        proxy.method_call(INTERFACE, "SetDiscoveryFilter", (filter.to_dict(),)).await?;
        proxy.method_call(INTERFACE, "StartDiscovery", ()).await?;

        let (term_tx, term_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _done_tx = done_tx;
            let _ = term_rx.await;

            let proxy = Proxy::new(SERVICE_NAME, &dbus_path, TIMEOUT, &*inner.connection);
            let _: std::result::Result<(), dbus::Error> = proxy.method_call(INTERFACE, "StopDiscovery", ()).await;
        });

        Ok(Self { adapter_name, _term_tx: term_tx })
    }
}

impl Drop for DeviceDiscovery {
    fn drop(&mut self) {
        // required for drop order
    }
}

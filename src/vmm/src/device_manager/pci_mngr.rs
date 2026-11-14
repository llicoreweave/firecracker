// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::path::Path;
use std::sync::{Arc, Mutex};

use event_manager::{MutEventSubscriber, SubscriberOps};
use serde::{Deserialize, Serialize};

use super::persist::{DeviceStates, MmdsState, VirtioDeviceState};
use crate::EventManager;
use crate::device_manager::DevicePersistError;
use crate::devices::pci::PciSegment;
use crate::devices::pci::root_port::{PciRootPortRestoreError, PciRootPortState};
use crate::devices::pci::static_topology::StaticTopology;
use crate::devices::vfio::pci::{BarRequirement, BarSlot, BarWindow, VfioPciDevice, VfioPciError};
use crate::devices::vfio::{VfioContext, VfioError};
use crate::devices::virtio::balloon::Balloon;
use crate::devices::virtio::balloon::persist::BalloonConstructorArgs;
use crate::devices::virtio::block::device::Block;
use crate::devices::virtio::block::persist::BlockConstructorArgs;
use crate::devices::virtio::device::{VirtioDevice, VirtioDeviceId, VirtioDeviceType};
use crate::devices::virtio::mem::VirtioMem;
use crate::devices::virtio::mem::persist::VirtioMemConstructorArgs;
use crate::devices::virtio::net::Net;
use crate::devices::virtio::net::persist::NetConstructorArgs;
use crate::devices::virtio::pmem::device::Pmem;
use crate::devices::virtio::pmem::persist::PmemConstructorArgs;
use crate::devices::virtio::rng::Entropy;
use crate::devices::virtio::rng::persist::EntropyConstructorArgs;
use crate::devices::virtio::transport::pci::device::{
    BarPlacement, CAPABILITY_BAR_SIZE, VirtioPciDevice, VirtioPciDeviceError, VirtioPciDeviceState,
};
use crate::devices::virtio::vsock::persist::{
    VsockConstructorArgs, VsockState, VsockUdsConstructorArgs,
};
use crate::devices::virtio::vsock::{Vsock, VsockUnixBackend};
use crate::logger::{debug, warn};
use crate::pci::PciSBDF;
use crate::pci::bus::PciBusError;
use crate::resources::VmResources;
use crate::seccomp::BpfProgram;
use crate::snapshot::Persist;
use crate::vmm_config::machine_config::MAX_PCIE_HOTPLUG_PORTS;
use crate::vmm_config::memory_hotplug::MemoryHotplugConfig;
use crate::vstate::bus::BusError;
use crate::vstate::interrupts::InterruptError;
use crate::vstate::memory::GuestMemoryMmap;
use crate::vstate::resources::ResourceAllocator;
use crate::vstate::vm::KvmVm;

/// Where on the PCI topology a virtio device should be placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciPlacement {
    /// Directly on the root/primary bus.
    PrimaryBus,
    /// In the slot of a free PCIe root port.
    SecondaryBus,
}

#[derive(Debug)]
pub struct PciDevices {
    /// PCIe segment of the VMM. We currently support a single PCIe segment.
    pub pci_segment: PciSegment,
    /// All VirtIO PCI devices of the system
    pub virtio_devices: HashMap<VirtioDeviceId, Arc<Mutex<VirtioPciDevice>>>,
    /// All VFIO passthrough PCI devices of the system, keyed by their Firecracker id.
    pub vfio_devices: HashMap<String, Arc<Mutex<VfioPciDevice>>>,
    /// The IOMMU context shared by the VFIO devices, if any is attached.
    pub vfio_context: Option<VfioContext>,
    pub(crate) topology: Option<StaticTopology>,
    topology_paths: BTreeMap<String, Vec<u8>>,
    topology_order: Vec<Vec<u8>>,
    numa: Option<crate::numa::NumaConfig>,
    topology_ranges: Vec<(Vec<u8>, u64, u64, bool)>,
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum PciManagerError {
    /// Invalid experimental static PCI topology: {0}
    Topology(String),
    /// Resource allocation error: {0}
    ResourceAllocation(#[from] vm_allocator::Error),
    /// Device {0} sets dmb_size_mib, which this transport does not support
    DmbNotSupported(String),
    /// Bus error: {0}
    Bus(#[from] BusError),
    /// PCI bus error: {0}
    PciBus(#[from] PciBusError),
    /// MSI error: {0}
    Msi(#[from] InterruptError),
    /// VirtIO PCI device error: {0}
    VirtioPciDevice(#[from] VirtioPciDeviceError),
    /// KVM error: {0}
    Kvm(#[from] vmm_sys_util::errno::Error),
    /// Could not restore a PCIe root port: {0}
    RootPort(#[from] PciRootPortRestoreError),
    /// Could not create the hot-unplug completion channel: {0}
    HotplugCompletion(std::io::Error),
    /// No PCIe root port is free. Every configured root port already has a
    /// device in it; raise `pcie_hotplug_ports` in the machine configuration to
    /// allow more hot-plugged devices.
    NoFreeRootPort,
    /// The snapshot has {0} PCIe root ports, but at most
    /// {MAX_PCIE_HOTPLUG_PORTS:} are supported.
    TooManyRootPorts(usize),
    /// The snapshot's PCIe root port at index {0} is inconsistent: it is at
    /// {1} and starts bus {2}.
    InconsistentRootPort(usize, PciSBDF, u8),
    /// VFIO error: {0}
    Vfio(#[from] VfioError),
    /// VFIO PCI device error: {0}
    VfioPci(#[from] VfioPciError),
}

impl PciDevices {
    pub fn new(vm: &Arc<KvmVm>, hotplug_ports: u8) -> Result<Self, PciManagerError> {
        // Currently we don't assign any IRQs to PCI devices. We will be using MSI-X interrupts
        // only.
        let pci_segment = PciSegment::new(0, vm, &[0u8; 32], hotplug_ports)?;

        Ok(Self {
            pci_segment,
            virtio_devices: HashMap::new(),
            vfio_devices: HashMap::new(),
            vfio_context: None,
            topology: None,
            topology_paths: BTreeMap::new(),
            topology_order: Vec::new(),
            numa: vm.common.numa.clone(),
            topology_ranges: Vec::new(),
        })
    }

    fn attach_common(
        &mut self,
        vm: &KvmVm,
        device_type: VirtioDeviceType,
        id: String,
        sbdf: PciSBDF,
        virtio_device: Arc<Mutex<VirtioPciDevice>>,
        event_manager: &mut EventManager,
    ) -> Result<(), PciManagerError> {
        let bar_address = {
            let mut device = virtio_device.lock().unwrap();

            device.register_notification_ioevents(vm)?;

            let sub_id = event_manager.add_subscriber(device.virtio_device());
            device.sub_id = Some(sub_id);

            device.bar_address()
        };

        self.virtio_devices
            .insert((device_type, id), virtio_device.clone());

        self.pci_segment
            .pci_buses
            .get(sbdf.bus())
            .ok_or_else(|| PciBusError::InvalidBusNumber(sbdf.bus()))?
            .lock()
            .expect("Poisoned lock")
            .add_device(sbdf.device(), virtio_device.clone())?;

        debug!(
            "Inserting MMIO BAR region: {:#x}:{:#x}",
            bar_address, CAPABILITY_BAR_SIZE
        );
        vm.common
            .mmio_bus
            .insert(virtio_device.clone(), bar_address, CAPABILITY_BAR_SIZE)?;

        Ok(())
    }

    pub(crate) fn attach_pci_virtio_device(
        &mut self,
        vm: &Arc<KvmVm>,
        id: String,
        device: Arc<Mutex<dyn VirtioDevice>>,
        event_manager: &mut EventManager,
        placement: PciPlacement,
    ) -> Result<(), PciManagerError> {
        if device
            .lock()
            .expect("Poisoned lock")
            .dmb_size_mib()
            .is_some()
        {
            return Err(PciManagerError::DmbNotSupported(id));
        }

        let (sbdf, root_port) = match placement {
            PciPlacement::PrimaryBus => (self.pci_segment.next_device_sbdf()?, None),
            PciPlacement::SecondaryBus => {
                let port = self.pci_segment.next_free_root_port()?;
                let secondary_bus = port.lock().expect("Poisoned lock").secondary_bus();
                let sbdf = PciSBDF::new(self.pci_segment.id, secondary_bus, 0, 0);
                (sbdf, Some(port))
            }
        };
        debug!("Allocating SBDF: {sbdf:?} for device");

        let device_type = device.lock().expect("Poisoned lock").device_type();

        // Allocate one MSI vector per queue, plus one for configuration
        let msix_num =
            u16::try_from(device.lock().expect("Poisoned lock").num_queues() + 1).unwrap();

        let msix_vectors = KvmVm::create_msix_group(vm.clone(), msix_num)?;

        // Create the transport
        let mut virtio_device =
            VirtioPciDevice::new(id.clone(), vm, device, Arc::new(msix_vectors), sbdf);

        // It's likely that the guest has already programmed the root port's
        // window. Try to place the device BAR inside that window if possible.
        let bar_placement = match root_port {
            None => BarPlacement::Packed,
            Some(port) => BarPlacement::BehindPort(
                port.lock().expect("Poisoned lock").nonpref_memory_window(),
            ),
        };

        // Don't hold the resource allocator lock across attach_common()
        // below: a device access holds the bus lock and can take the allocator
        // lock, so the reverse order can deadlock.
        virtio_device.allocate_bars(&mut vm.resource_allocator().mmio32_memory, bar_placement)?;

        let virtio_device = Arc::new(Mutex::new(virtio_device));

        self.attach_common(vm, device_type, id, sbdf, virtio_device, event_manager)
    }

    pub(crate) fn pci_segment(&self) -> &PciSegment {
        &self.pci_segment
    }

    /// Attach the configured number of PCIe root ports.
    pub(crate) fn attach_root_ports(&mut self, vm: &Arc<KvmVm>) -> Result<(), PciManagerError> {
        // Static buses and bridges have already been materialized from the supplied graph.
        // The segment's hotplug path interprets secondary buses as reserved root ports.
        if self.topology.is_some() {
            return Ok(());
        }
        self.pci_segment.attach_root_ports(vm)
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn append_aml_bytes(
        &self,
        dsdt_data: &mut Vec<u8>,
    ) -> Result<(), acpi_tables::aml::AmlError> {
        use acpi_tables::Aml;

        if let Some(plan) = &self.topology {
            let segment = self.pci_segment();
            for root in &plan.config.roots {
                let windows = |prefetch| {
                    pci_root_windows(
                        plan,
                        &self.topology_ranges,
                        root.bus,
                        prefetch,
                        if prefetch {
                            (segment.start_of_mem64_area, segment.end_of_mem64_area)
                        } else {
                            (segment.start_of_mem32_area, segment.end_of_mem32_area)
                        },
                    )
                };
                PciSegment::append_configured_root(
                    dsdt_data,
                    segment.mmio_config_address,
                    root,
                    &windows(false),
                    &windows(true),
                    &segment.pci_irq_slots,
                )?;
            }
            return Ok(());
        }
        self.pci_segment().append_aml_bytes(dsdt_data)
    }

    pub(crate) fn detach_pci_virtio_device(
        &mut self,
        vm: &KvmVm,
        device_id: VirtioDeviceId,
        event_manager: &mut EventManager,
    ) -> Result<(), PciManagerError> {
        let pci_device_arc = self
            .virtio_devices
            .remove(&device_id)
            .expect("device presence should be checked before detach");

        let sbdf = pci_device_arc.lock().expect("Poisoned lock").sbdf;

        // If the device is behind a root port, notify the guest.
        if let Some(port) = self.pci_segment.root_port_for_bus(sbdf.bus()) {
            port.lock().expect("Poisoned lock").eject();
        }

        // Remove the device from the PCI bus first. A config space access runs
        // with the PCI bus lock held and can relocate the BAR, so afterwards
        // the BAR address of the device can no longer change under us.
        self.pci_segment
            .pci_buses
            .get(sbdf.bus())
            .ok_or_else(|| PciBusError::InvalidBusNumber(sbdf.bus()))?
            .lock()
            .expect("Poisoned lock")
            .remove_device(sbdf.device());

        // Next operations of removing device from mmio_bus and pci_bus need to wait for any other
        // user of the device to finish. This requires us to not hold the lock for the device in
        // case someone will try to access the device while we are in these several lines of code.
        let (bar_addr, sub_id) = {
            let pci_device = pci_device_arc.lock().expect("Poisoned lock");

            pci_device
                .unregister_notification_ioevents(vm)
                .map_err(PciManagerError::Kvm)?;
            (pci_device.bar_address(), pci_device.sub_id)
        };

        vm.common
            .mmio_bus
            .remove(bar_addr, CAPABILITY_BAR_SIZE)
            .map_err(PciManagerError::Bus)?;

        if let Some(sub_id) = sub_id
            && event_manager.remove_subscriber(sub_id).is_err()
        {
            warn!("Failed to remove event subscriber for device {device_id:?}");
        }

        // The device goes away, so the queue pages it wrote since the last snapshot have to be
        // committed now, or the next snapshot misses them.
        let mut pci_device = pci_device_arc.lock().expect("Poisoned lock");
        pci_device
            .virtio_device()
            .lock()
            .expect("Poisoned lock")
            .commit_dirty();

        pci_device.free_bars(&mut vm.resource_allocator().mmio32_memory);

        // Ensure no other references to the device remain, so it is freed when
        // this function returns.
        assert_eq!(Arc::strong_count(&pci_device_arc), 1);

        Ok(())
    }

    /// Ask the guest to release a device.
    pub(crate) fn request_unplug(&self, device_id: &VirtioDeviceId) {
        let bus = self
            .virtio_devices
            .get(device_id)
            .expect("device presence should be checked before unplug")
            .lock()
            .expect("Poisoned lock")
            .sbdf
            .bus();

        if let Some(port) = self.pci_segment.root_port_for_bus(bus) {
            port.lock().expect("Poisoned lock").request_unplug();
        }
    }

    /// Assign the host PCI functions described by `configs`, pairs of a Firecracker id and the
    /// host sysfs path of the function (e.g. `/sys/bus/pci/devices/0000:01:00.0`), to the guest.
    ///
    /// All functions are attached at once: they share one IOMMU context, and the BARs of all of
    /// them are placed together, largest first and from the top of each MMIO window, which packs
    /// naturally aligned power-of-two BARs without fragmentation and keeps them clear of the
    /// virtio-pci BARs allocated from the bottom of the window.
    pub(crate) fn attach_vfio_devices(
        &mut self,
        vm: &Arc<KvmVm>,
        configs: &[(String, &Path)],
    ) -> Result<(), PciManagerError> {
        if configs.is_empty() {
            if let Some(plan) = &self.topology {
                plan.install(&self.pci_segment.pci_buses, &[])?;
            }
            return Ok(());
        }
        // Only one attach is supported: all the devices must share a single IOMMU context.
        assert!(self.vfio_context.is_none());

        let paths: Vec<&Path> = configs.iter().map(|&(_, path)| path).collect();
        let (context, opened) = VfioContext::new(vm, &paths)?;
        self.add_vfio_devices(
            vm,
            configs.iter().zip(opened).map(|((id, _), opened)| {
                move |sbdf: PciSBDF| VfioPciDevice::new(id.clone(), sbdf, opened, vm.clone())
            }),
        )?;
        self.vfio_context = Some(context);

        Ok(())
    }

    /// Validate the requested tree before any host VFIO devices are opened.
    pub fn configure_static_topology(
        &mut self,
        topology: &crate::vmm_config::pci_topology::PciTopologyConfig,
        configs: &[crate::vmm_config::vfio::VfioDeviceConfig],
    ) -> Result<(), PciManagerError> {
        if !self.pci_segment.root_ports.is_empty() {
            return Err(PciManagerError::Topology(
                "static PCI topology cannot use hotplug ports".into(),
            ));
        }
        let plan = StaticTopology::new(
            topology,
            configs,
            self.numa.as_ref().map_or(0, |n| n.nodes.len()),
        )
        .map_err(PciManagerError::Topology)?;
        for address in topology
            .bridges
            .iter()
            .map(|b| b.pci_address)
            .chain(configs.iter().filter_map(|c| c.pci_address))
        {
            if self
                .pci_segment
                .pci_buses
                .get(address.bus)
                .is_some_and(|bus| bus.lock().unwrap().get_device(address.device).is_some())
            {
                return Err(PciManagerError::Topology(
                    "configured PCI address is already occupied".into(),
                ));
            }
        }
        self.topology_paths = plan.paths.clone();
        self.topology_order = configs.iter().map(|c| plan.paths[&c.id].clone()).collect();
        self.topology = Some(plan);
        Ok(())
    }

    /// Add passthrough devices to the PCI bus, each built by its constructor at the SBDF it is
    /// given, then place and map the BARs of all of them.
    fn add_vfio_devices<F>(
        &mut self,
        vm: &Arc<KvmVm>,
        constructors: impl IntoIterator<Item = F>,
    ) -> Result<(), PciManagerError>
    where
        F: FnOnce(PciSBDF) -> Result<VfioPciDevice, VfioPciError>,
    {
        let plan = self.topology.as_ref().ok_or_else(|| {
            PciManagerError::Topology("VFIO requires explicit pci-topology".into())
        })?;
        let mut devices = Vec::new();
        let mut intx_gsis = IntxGsis::default();
        for (index, constructor) in constructors.into_iter().enumerate() {
            // Constructors follow the caller's configuration order.
            let path = self
                .topology_order
                .get(index)
                .ok_or_else(|| PciManagerError::Topology("missing endpoint address".into()))?;
            let sbdf = plan.address(path);
            let mut device = constructor(sbdf)?;
            debug!("Allocating SBDF: {sbdf:?} for VFIO device {}", device.id());
            if device.supports_intx() && plan.config.bridges.is_empty() {
                if sbdf.bus() != 0 {
                    return Err(PciManagerError::Topology(
                        "INTx requires root bus zero".into(),
                    ));
                }
                let gsi = intx_gsis.next(&mut vm.resource_allocator())?;
                debug!("vfio: routing INTx of {} to GSI {gsi}", device.id());
                device.route_intx(gsi)?;
            }
            let device = Arc::new(Mutex::new(device));
            devices.push(device);
        }

        let requirements: Vec<_> = devices
            .iter()
            .enumerate()
            .flat_map(|(device, vfio)| {
                vfio.lock()
                    .expect("Poisoned lock")
                    .bar_requirements()
                    .into_iter()
                    .map(move |requirement| (device, requirement))
            })
            .collect();
        let placements = if plan.config.roots.len() > 1 || !plan.config.bridges.is_empty() {
            place_grouped_vfio_bars(
                &mut vm.resource_allocator(),
                requirements,
                &self.topology_order,
                self.topology.as_ref(),
            )?
        } else {
            place_vfio_bars(&mut vm.resource_allocator(), requirements)?
        };
        let mut topology_ranges = Vec::new();
        for (device, slot, guest_addr) in placements {
            if self.topology.is_some() {
                let locked = devices[device].lock().unwrap();
                let requirement = locked
                    .bar_requirements()
                    .into_iter()
                    .find(|r| r.slot == slot)
                    .unwrap();
                topology_ranges.push((
                    self.topology_paths[locked.id()].clone(),
                    guest_addr,
                    requirement.size,
                    requirement.window == BarWindow::Mmio64,
                ));
            }
            devices[device]
                .lock()
                .expect("Poisoned lock")
                .place_bar(slot, guest_addr);
        }

        if let Some(plan) = &self.topology {
            plan.install(&self.pci_segment.pci_buses, &topology_ranges)?;
            for device in &devices {
                let bdf = device.lock().unwrap().sbdf();
                self.pci_segment
                    .pci_buses
                    .get(bdf.bus())
                    .ok_or_else(|| PciBusError::InvalidBusNumber(bdf.bus()))?
                    .lock()
                    .unwrap()
                    .add_device(bdf.device(), device.clone())?;
            }
        }
        self.topology_ranges = topology_ranges;

        #[cfg(target_arch = "x86_64")]
        {
            // The ACPI PCI routing table of the segment maps INTA of each slot to its GSI.
            let pci_segment = &mut self.pci_segment;
            for device in &devices {
                let vfio = device.lock().expect("Poisoned lock");
                if let Some(gsi) = vfio.intx_gsi() {
                    pci_segment.pci_irq_slots[usize::from(vfio.sbdf().device())] =
                        u8::try_from(gsi).expect("x86 legacy GSIs fit in a byte");
                }
            }
        }

        for device in devices {
            let (id, ranges) = {
                let mut vfio = device.lock().expect("Poisoned lock");
                vfio.map_bars()?;
                (vfio.id().to_string(), vfio.mmio_ranges())
            };
            for (base, len) in ranges {
                debug!("vfio: trapping BAR range {base:#x}:{len:#x} on the MMIO bus");
                vm.common.mmio_bus.insert(device.clone(), base, len)?;
            }
            self.vfio_devices.insert(id, device);
        }

        Ok(())
    }

    /// Whether the cold-boot graph has state that is absent from snapshots.
    pub fn has_configured_topology(&self) -> bool {
        self.topology.is_some()
    }

    /// Whether any VFIO passthrough device is currently attached.
    pub fn has_vfio_devices(&self) -> bool {
        !self.vfio_devices.is_empty()
    }

    /// The `(slot, GSI)` routes of the INTA pins of the devices on the PCI bus, by slot.
    pub fn intx_routes(&self) -> Vec<(u8, u32)> {
        let mut routes: Vec<(u8, u32)> = self
            .vfio_devices
            .values()
            .filter_map(|device| {
                let device = device.lock().expect("Poisoned lock");
                device.intx_gsi().map(|gsi| (device.sbdf().device(), gsi))
            })
            .collect();
        routes.sort_unstable();
        routes
    }

    fn restore_pci_device<T: 'static + VirtioDevice + MutEventSubscriber + Debug>(
        &mut self,
        vm: &Arc<KvmVm>,
        device: Arc<Mutex<T>>,
        device_id: &str,
        transport_state: &VirtioPciDeviceState,
        event_manager: &mut EventManager,
    ) -> Result<(), PciManagerError> {
        let device_type = device.lock().expect("Poisoned lock").device_type();

        let virtio_device = Arc::new(Mutex::new(VirtioPciDevice::new_from_state(
            device_id.to_string(),
            vm,
            device.clone(),
            transport_state.clone(),
        )?));

        self.attach_common(
            vm,
            device_type,
            device_id.to_string(),
            transport_state.sbdf,
            virtio_device,
            event_manager,
        )?;

        Ok(())
    }

    /// Gets the specified device.
    pub fn get_virtio_device(
        &self,
        device_type: VirtioDeviceType,
        device_id: &str,
    ) -> Option<&Arc<Mutex<VirtioPciDevice>>> {
        self.virtio_devices
            .get(&(device_type, device_id.to_string()))
    }

    /// Return whether the device sits behind a PCIe root port, and is
    /// therefore hot-unpluggable.
    pub(crate) fn is_removable(&self, device_type: VirtioDeviceType, device_id: &str) -> bool {
        self.get_virtio_device(device_type, device_id)
            .is_some_and(|device| device.lock().expect("Poisoned lock").sbdf.bus() != 0)
    }

    pub(crate) fn get_device(
        &self,
        device_type: VirtioDeviceType,
        device_id: &str,
    ) -> Option<Arc<Mutex<dyn VirtioDevice>>> {
        self.get_virtio_device(device_type, device_id)
            .map(|device| device.lock().expect("Poisoned lock").virtio_device())
    }

    pub(crate) fn contains_virtio_device(&self, device_id: &VirtioDeviceId) -> bool {
        self.virtio_devices.contains_key(device_id)
    }

    pub fn for_each_virtio_device(&self, mut f: impl FnMut(VirtioDeviceType, &dyn VirtioDevice)) {
        for ((device_type, _), pci_device) in &self.virtio_devices {
            let device_arc = pci_device.lock().expect("Poisoned lock").virtio_device();
            let device = device_arc.lock().expect("Poisoned lock");
            f(*device_type, &*device);
        }
    }

    pub(crate) fn for_each_virtio_device_mut(
        &self,
        mut f: impl FnMut(VirtioDeviceType, &mut dyn VirtioDevice),
    ) {
        for ((device_type, _), pci_device) in &self.virtio_devices {
            let device_arc = pci_device.lock().expect("Poisoned lock").virtio_device();
            let mut device = device_arc.lock().expect("Poisoned lock");
            f(*device_type, &mut *device);
        }
    }
}

/// First GSI INTx pins are routed to. On x86, the IOAPIC inputs from 16 up: KVM also wires GSIs 0
/// to 15 to the 8259 PIC, and PC chipsets route PCI interrupts to inputs 16 to 23 as well.
#[cfg(target_arch = "x86_64")]
const INTX_GSI_START: u32 = 16;
#[cfg(target_arch = "aarch64")]
const INTX_GSI_START: u32 = crate::arch::GSI_LEGACY_START;

/// The GSIs the INTx pins of passthrough devices are routed to: a GSI of its own for each device
/// while free ones are left, then the GSIs already used, in turn. INTx is level-triggered, so
/// devices can share a GSI, as they share interrupt lines on physical platforms.
#[derive(Debug, Default)]
struct IntxGsis {
    own: Vec<u32>,
    shared: usize,
}

impl IntxGsis {
    fn next(&mut self, allocator: &mut ResourceAllocator) -> Result<u32, vm_allocator::Error> {
        match allocator.allocate_gsi_legacy_from(INTX_GSI_START) {
            Ok(gsi) => {
                self.own.push(gsi);
                Ok(gsi)
            }
            Err(err) if self.own.is_empty() => Err(err),
            Err(_) => {
                let gsi = self.own[self.shared % self.own.len()];
                self.shared += 1;
                Ok(gsi)
            }
        }
    }
}

/// Choose the guest addresses of VFIO BARs, given as `(device, requirement)` pairs.
///
/// BARs are naturally aligned powers of two. Placing them largest first, each at the highest
/// suitable address of its window, packs them without fragmentation below the top of the window,
/// away from the virtio-pci BARs which are allocated from the bottom.
#[cfg(test)]
fn place_static_vfio_bars(
    allocator: &mut ResourceAllocator,
    requirements: Vec<(usize, BarRequirement)>,
    roots: &[u8],
) -> Result<Vec<(usize, BarSlot, u64)>, vm_allocator::Error> {
    let paths: Vec<_> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| vec![*root, u8::try_from(i).unwrap()])
        .collect();
    place_grouped_vfio_bars(allocator, requirements, &paths, None)
}

struct SubtreeBars {
    size: u64,
    alignment: u64,
    entries: Vec<(usize, BarSlot, u64)>,
}

fn align_bar(value: u64, alignment: u64) -> Result<u64, vm_allocator::Error> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
        .ok_or(vm_allocator::Error::ResourceNotAvailable)
}

/// Plan bottom-up so sibling bridges never decode each other's BARs, including
/// the mandatory 1-MiB bridge window granularity. Only assigned endpoints enter
/// this tree; physical bridges and unassigned host functions are never opened.
fn subtree_bars(
    requirements: &[(usize, BarRequirement)],
    paths: &[Vec<u8>],
    depth: usize,
) -> Result<SubtreeBars, vm_allocator::Error> {
    let mut result = SubtreeBars {
        size: 0,
        alignment: 1 << 20,
        entries: Vec::new(),
    };
    if paths[requirements[0].0].len() == depth {
        let mut bars = requirements.to_vec();
        bars.sort_by_key(|(_, b)| std::cmp::Reverse(b.size));
        for (device, bar) in bars {
            result.alignment = result.alignment.max(bar.size);
            result.size = align_bar(result.size, bar.size)?;
            result.entries.push((device, bar.slot, result.size));
            result.size = result
                .size
                .checked_add(bar.size)
                .ok_or(vm_allocator::Error::ResourceNotAvailable)?;
        }
    } else {
        let mut children = BTreeMap::<u8, Vec<(usize, BarRequirement)>>::new();
        for &(device, bar) in requirements {
            children
                .entry(paths[device][depth])
                .or_default()
                .push((device, bar));
        }
        let mut layouts = children
            .values()
            .map(|bars| subtree_bars(bars, paths, depth + 1))
            .collect::<Result<Vec<_>, _>>()?;
        layouts.sort_by_key(|child| std::cmp::Reverse((child.alignment, child.size)));
        for child in layouts {
            result.alignment = result.alignment.max(child.alignment);
            let base = align_bar(result.size, child.alignment)?;
            result.size = base
                .checked_add(child.size)
                .ok_or(vm_allocator::Error::ResourceNotAvailable)?;
            result.entries.extend(
                child
                    .entries
                    .into_iter()
                    .map(|(dev, slot, offset)| (dev, slot, base + offset)),
            );
        }
    }
    result.size = align_bar(result.size, 1 << 20)?;
    Ok(result)
}

/// Reserve consecutive windows by configured root bus, even when root slots are
/// interleaved. Exact placement prevents filling a previous root's gaps.
fn place_grouped_vfio_bars(
    allocator: &mut ResourceAllocator,
    requirements: Vec<(usize, BarRequirement)>,
    paths: &[Vec<u8>],
    plan: Option<&StaticTopology>,
) -> Result<Vec<(usize, BarSlot, u64)>, vm_allocator::Error> {
    let mut groups = BTreeMap::<(u8, u8), Vec<(usize, BarRequirement)>>::new();
    for (device, bar) in requirements {
        let (bus, root) = if plan.is_some() {
            (paths[device][0], paths[device][1])
        } else {
            (0, paths[device][0])
        };
        groups.entry((bus, root)).or_default().push((device, bar));
    }
    let mut next = [None, None];
    let mut placed = Vec::new();
    for bars in groups.values() {
        for (index, kind) in [BarWindow::Mmio32, BarWindow::Mmio64]
            .into_iter()
            .enumerate()
        {
            let bars: Vec<_> = bars
                .iter()
                .filter(|(_, b)| b.window == kind)
                .copied()
                .collect();
            if bars.is_empty() {
                continue;
            }
            let layout = subtree_bars(&bars, paths, if plan.is_some() { 2 } else { 1 })?;
            let parent = match kind {
                BarWindow::Mmio32 => &mut allocator.mmio32_memory,
                BarWindow::Mmio64 => &mut allocator.mmio64_memory,
            };
            let policy = match next[index] {
                Some(cursor) => {
                    vm_allocator::AllocPolicy::ExactMatch(align_bar(cursor, layout.alignment)?)
                }
                None => vm_allocator::AllocPolicy::FirstMatch,
            };
            let range = parent.allocate(layout.size, layout.alignment, policy)?;
            next[index] = range.end().checked_add(1);
            placed.extend(
                layout
                    .entries
                    .into_iter()
                    .map(|(dev, slot, offset)| (dev, slot, range.start() + offset)),
            );
        }
    }
    Ok(placed)
}

/// ACPI root windows are disjoint and cover each bridge's 1-MiB decoding
/// granularity. Root zero owns the remaining aperture, including virtio BARs.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
fn pci_root_windows(
    plan: &StaticTopology,
    ranges: &[(Vec<u8>, u64, u64, bool)],
    root_bus: u8,
    prefetch: bool,
    aperture: (u64, u64),
) -> Vec<(u64, u64)> {
    let span = |root_bus: u8| {
        let selected: Vec<_> = ranges
            .iter()
            .filter(|(path, _, _, pref)| *pref == prefetch && path[0] == root_bus)
            .collect();
        Some((
            selected.iter().map(|r| r.1).min()? & !((1 << 20) - 1),
            selected.iter().map(|r| r.1 + r.2 - 1).max()? | ((1 << 20) - 1),
        ))
    };
    if root_bus != 0 {
        return span(root_bus).into_iter().collect();
    }
    let mut holes: Vec<_> = plan
        .config
        .roots
        .iter()
        .filter(|r| r.bus != 0)
        .filter_map(|r| span(r.bus))
        .collect();
    holes.sort_unstable();
    let mut result = Vec::new();
    let (mut next, end) = aperture;
    for (low, high) in holes {
        if next < low {
            result.push((next, low - 1));
        }
        next = high + 1;
    }
    if next <= end {
        result.push((next, end));
    }
    result
}

#[cfg(test)]
mod static_placement_tests {
    use super::*;

    #[test]
    fn static_placement_nested_siblings_and_interleaved_numa_roots() {
        use crate::vmm_config::pci_topology::*;
        let paths = vec![
            vec![0, 1, 0, 1, 0],
            vec![20, 2, 0],
            vec![10, 3, 0],
            vec![0, 1, 0, 2, 0],
            vec![0, 1, 0, 1, 1],
        ];
        let config = PciTopologyConfig {
            roots: vec![
                PciRootConfig {
                    bus: 0,
                    last_bus: 9,
                    numa_node: Some(2),
                },
                PciRootConfig {
                    bus: 20,
                    last_bus: 29,
                    numa_node: Some(0),
                },
                PciRootConfig {
                    bus: 10,
                    last_bus: 19,
                    numa_node: Some(0),
                },
            ],
            bridges: vec![
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 0,
                        device: 1,
                        function: 0,
                    },
                    secondary_bus: 1,
                    subordinate_bus: 4,
                    kind: PciBridgeKind::RootPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 1,
                        device: 0,
                        function: 0,
                    },
                    secondary_bus: 2,
                    subordinate_bus: 4,
                    kind: PciBridgeKind::UpstreamPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 2,
                        device: 1,
                        function: 0,
                    },
                    secondary_bus: 3,
                    subordinate_bus: 3,
                    kind: PciBridgeKind::DownstreamPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 2,
                        device: 2,
                        function: 0,
                    },
                    secondary_bus: 4,
                    subordinate_bus: 4,
                    kind: PciBridgeKind::DownstreamPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 20,
                        device: 2,
                        function: 0,
                    },
                    secondary_bus: 21,
                    subordinate_bus: 21,
                    kind: PciBridgeKind::RootPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 10,
                        device: 3,
                        function: 0,
                    },
                    secondary_bus: 11,
                    subordinate_bus: 11,
                    kind: PciBridgeKind::RootPort,
                },
            ],
        };
        let plan = StaticTopology::new(&config, &[], 3).unwrap();
        let requirements: Vec<_> = (0..paths.len())
            .map(|device| {
                (
                    device,
                    BarRequirement {
                        slot: BarSlot::Bar(0),
                        size: 4096,
                        window: BarWindow::Mmio64,
                    },
                )
            })
            .collect();
        let mut allocator = ResourceAllocator::new();
        let placed =
            place_grouped_vfio_bars(&mut allocator, requirements, &paths, Some(&plan)).unwrap();
        let address = |dev| placed.iter().find(|(d, _, _)| *d == dev).unwrap().2;
        // The neighboring downstream bridge must not decode either endpoint
        // on the first bridge's bus, even with interleaved configuration order.
        assert!(
            address(3) < address(0).min(address(4))
                || address(3) >= address(0).max(address(4)) + (1 << 20)
        );
        let ranges: Vec<_> = placed
            .iter()
            .map(|(dev, _, base)| (paths[*dev].clone(), *base, 4096, true))
            .collect();
        let windows: Vec<_> = [0, 10, 20]
            .into_iter()
            .map(|domain| {
                pci_root_windows(
                    &plan,
                    &ranges,
                    domain,
                    true,
                    (
                        allocator.mmio64_memory.base(),
                        allocator.mmio64_memory.end(),
                    ),
                )
            })
            .collect();
        for first in 0..3 {
            for second in first + 1..3 {
                for &(a, b) in &windows[first] {
                    for &(c, d) in &windows[second] {
                        assert!(b < c || d < a);
                    }
                }
            }
        }
        assert!(address(2) < address(1)); // Configured root bus order, independent of NUMA affinity.
    }

    #[test]
    fn static_placement_flat_multiple_roots_have_disjoint_bar_windows() {
        use crate::vmm_config::pci_topology::*;
        let config = PciTopologyConfig {
            roots: [0, 20, 40]
                .map(|bus| PciRootConfig {
                    bus,
                    last_bus: bus,
                    numa_node: None,
                })
                .to_vec(),
            bridges: vec![],
        };
        let plan = StaticTopology::new(&config, &[], 0).unwrap();
        let paths = vec![vec![0, 1], vec![20, 1], vec![40, 1]];
        let requirements: Vec<_> = [
            (0, 16 << 20),
            (0, 1 << 20),
            (1, 8 << 20),
            (1, 4 << 20),
            (2, 2 << 20),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (device, size))| {
            (
                device,
                BarRequirement {
                    slot: BarSlot::Bar(u8::try_from(i).unwrap()),
                    size,
                    window: BarWindow::Mmio64,
                },
            )
        })
        .collect();
        for (base, size) in [
            (crate::arch::MMIO64_MEM_START, crate::arch::MMIO64_MEM_SIZE),
            (16 << 30, 8 << 30),
            (2 << 40, 2 << 40),
        ] {
            let mut allocator = ResourceAllocator::with_mmio64_range(base, size).unwrap();
            let placed =
                place_grouped_vfio_bars(&mut allocator, requirements.clone(), &paths, Some(&plan))
                    .unwrap();
            let ranges: Vec<_> = placed
                .iter()
                .map(|(dev, slot, base)| {
                    (
                        paths[*dev].clone(),
                        *base,
                        requirements
                            .iter()
                            .find(|(d, b)| d == dev && b.slot == *slot)
                            .unwrap()
                            .1
                            .size,
                        true,
                    )
                })
                .collect();
            let windows: Vec<_> = [0, 20, 40]
                .map(|bus| {
                    pci_root_windows(
                        &plan,
                        &ranges,
                        bus,
                        true,
                        (
                            allocator.mmio64_memory.base(),
                            allocator.mmio64_memory.end(),
                        ),
                    )
                })
                .to_vec();
            for (path, base, size, _) in &ranges {
                let domain = match path[0] {
                    0 => 0,
                    20 => 1,
                    _ => 2,
                };
                assert!(
                    windows[domain]
                        .iter()
                        .any(|(low, high)| low <= base && base + size - 1 <= *high)
                );
            }
            assert!(
                ranges
                    .iter()
                    .all(|(_, addr, len, _)| { *addr >= base && *addr + *len <= base + size })
            );
            assert!(
                windows
                    .iter()
                    .flatten()
                    .all(|&(low, high)| { low >= base && high < base + size })
            );
            for first in 0..3 {
                for second in first + 1..3 {
                    for &(a, b) in &windows[first] {
                        for &(c, d) in &windows[second] {
                            assert!(b < c || d < a);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn static_placement_reports_exhausted_aperture() {
        let mut allocator = ResourceAllocator::new();
        allocator.mmio64_memory = vm_allocator::AddressAllocator::new(1 << 40, 1 << 20).unwrap();
        place_static_vfio_bars(
            &mut allocator,
            vec![(
                0,
                BarRequirement {
                    slot: BarSlot::Bar(0),
                    size: 2 << 20,
                    window: BarWindow::Mmio64,
                },
            )],
            &[1],
        )
        .unwrap_err();
    }
    #[test]
    fn static_placement_full_fabric_sibling_windows_do_not_overlap() {
        let mut allocator = ResourceAllocator::with_mmio64_range(2 << 40, 2 << 40).unwrap();
        allocator
            .mmio32_memory
            .allocate(13 * 4096, 4096, vm_allocator::AllocPolicy::FirstMatch)
            .unwrap();
        let mut requirements = Vec::new();
        let mut roots = Vec::new();
        for root in [13, 14, 15, 16, 18, 19, 20, 21] {
            let gpu = roots.len();
            roots.push(root);
            for (index, size) in [(0, 16 << 20), (2, 128 << 30), (4, 32 << 20)] {
                requirements.push((
                    gpu,
                    BarRequirement {
                        slot: BarSlot::Bar(index),
                        size,
                        window: BarWindow::Mmio64,
                    },
                ));
            }
            let nic = roots.len();
            roots.push(root);
            requirements.push((
                nic,
                BarRequirement {
                    slot: BarSlot::Bar(0),
                    size: 32 << 20,
                    window: BarWindow::Mmio64,
                },
            ));
        }
        for _ in 0..4 {
            let switch = roots.len();
            roots.push(17);
            requirements.push((
                switch,
                BarRequirement {
                    slot: BarSlot::Bar(0),
                    size: 32 << 20,
                    window: BarWindow::Mmio32,
                },
            ));
        }
        // Eight 128 GiB BARs cannot fit the default 256 GiB aperture.
        place_static_vfio_bars(&mut ResourceAllocator::new(), requirements.clone(), &roots)
            .unwrap_err();
        let placed = place_static_vfio_bars(&mut allocator, requirements.clone(), &roots).unwrap();
        assert_eq!(placed.len(), 36);
        for gpu in (0..16).step_by(2) {
            let span = |device| {
                let bars: Vec<_> = placed.iter().filter(|(d, _, _)| *d == device).collect();
                let begin = bars.iter().map(|(_, _, base)| *base).min().unwrap();
                let end = bars
                    .iter()
                    .map(|(_, slot, base)| {
                        base + requirements
                            .iter()
                            .find(|(d, r)| *d == device && r.slot == *slot)
                            .unwrap()
                            .1
                            .size
                            - 1
                    })
                    .max()
                    .unwrap();
                (begin, end)
            };
            let (gpu_start, gpu_end) = span(gpu);
            let (nic_start, nic_end) = span(gpu + 1);
            assert!(gpu_end < nic_start || nic_end < gpu_start);
        }
    }
}

fn place_vfio_bars(
    allocator: &mut ResourceAllocator,
    mut requirements: Vec<(usize, BarRequirement)>,
) -> Result<Vec<(usize, BarSlot, u64)>, vm_allocator::Error> {
    // Stable sort: equally sized BARs keep the device and BAR order.
    requirements.sort_by_key(|(_, requirement)| std::cmp::Reverse(requirement.size));
    requirements
        .into_iter()
        .map(|(device, requirement)| {
            let window = match requirement.window {
                BarWindow::Mmio32 => &mut allocator.mmio32_memory,
                BarWindow::Mmio64 => &mut allocator.mmio64_memory,
            };
            let range = window.allocate(
                requirement.size,
                requirement.size,
                vm_allocator::AllocPolicy::LastMatch,
            )?;
            Ok((device, requirement.slot, range.start()))
        })
        .collect()
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct PciDevicesState {
    /// Virtio device states.
    pub virtio_devices: DeviceStates<VirtioPciDeviceState>,
    /// PCIe root port states.
    #[serde(default)]
    pub root_ports: Vec<PciRootPortState>,
}

pub struct PciDevicesConstructorArgs<'a> {
    pub vm: &'a Arc<KvmVm>,
    pub mem: &'a GuestMemoryMmap,
    pub vm_resources: &'a mut VmResources,
    pub instance_id: &'a str,
    pub event_manager: &'a mut EventManager,
    pub blk_worker_filter: Option<Arc<BpfProgram>>,
}

impl<'a> Debug for PciDevicesConstructorArgs<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PciDevicesConstructorArgs")
            .field("vm", &self.vm)
            .field("mem", &self.mem)
            .field("vm_resources", &self.vm_resources)
            .field("instance_id", &self.instance_id)
            .finish()
    }
}

impl<'a> Persist<'a> for PciDevices {
    type State = PciDevicesState;
    type ConstructorArgs = PciDevicesConstructorArgs<'a>;
    type Error = DevicePersistError;

    fn save(&self) -> Self::State {
        let mut state = DeviceStates::default();

        for pci_dev in self.virtio_devices.values() {
            let locked_pci_dev = pci_dev.lock().expect("Poisoned lock");
            let virtio_dev = locked_pci_dev.virtio_device();
            // We need to call `prepare_save()` on the device before saving the transport
            // so that, if we modify the transport state while preparing the device, e.g. sending
            // an interrupt to the guest, this is correctly captured in the saved transport state.
            let mut locked_virtio_dev = virtio_dev.lock().expect("Poisoned lock");
            locked_virtio_dev.prepare_save();
            let transport_state = locked_pci_dev.state();

            match locked_virtio_dev.device_type() {
                VirtioDeviceType::Balloon => {
                    let balloon_device = locked_virtio_dev
                        .as_any()
                        .downcast_ref::<Balloon>()
                        .unwrap();

                    let device_state = balloon_device.save();

                    state.balloon_device = Some(VirtioDeviceState {
                        device_id: balloon_device.id().to_string(),
                        device_state,
                        transport_state,
                    });
                }
                VirtioDeviceType::Block => {
                    let block_dev = locked_virtio_dev
                        .as_mut_any()
                        .downcast_mut::<Block>()
                        .unwrap();
                    if block_dev.is_vhost_user() {
                        warn!(
                            "Skipping vhost-user-block device. VhostUserBlock does not support \
                             snapshotting yet"
                        );
                    } else {
                        let device_state = block_dev.save();
                        state.block_devices.push(VirtioDeviceState {
                            device_id: block_dev.id().to_string(),
                            device_state,
                            transport_state,
                        });
                    }
                }
                VirtioDeviceType::Net => {
                    let net_dev = locked_virtio_dev
                        .as_mut_any()
                        .downcast_mut::<Net>()
                        .unwrap();
                    if let (Some(mmds_ns), None) = (net_dev.mmds_ns.as_ref(), state.mmds.as_ref()) {
                        let mmds_guard = mmds_ns.mmds.lock().expect("Poisoned lock");
                        state.mmds = Some(MmdsState {
                            version: mmds_guard.version(),
                            imds_compat: mmds_guard.imds_compat(),
                        });
                    }
                    let device_state = net_dev.save();

                    state.net_devices.push(VirtioDeviceState {
                        device_id: net_dev.id().to_string(),
                        device_state,
                        transport_state,
                    })
                }
                VirtioDeviceType::Vsock => {
                    let vsock_dev = locked_virtio_dev
                        .as_mut_any()
                        // Currently, VsockUnixBackend is the only implementation of VsockBackend.
                        .downcast_mut::<Vsock<VsockUnixBackend>>()
                        .unwrap();

                    // Save state after potential notification to the guest. This
                    // way we save changes to the queue the notification can cause.
                    let vsock_state = VsockState {
                        backend: vsock_dev.backend().save(),
                        frontend: vsock_dev.save(),
                    };

                    state.vsock_device = Some(VirtioDeviceState {
                        device_id: vsock_dev.id().to_string(),
                        device_state: vsock_state,
                        transport_state,
                    });
                }
                VirtioDeviceType::Rng => {
                    let rng_dev = locked_virtio_dev
                        .as_mut_any()
                        .downcast_mut::<Entropy>()
                        .unwrap();
                    let device_state = rng_dev.save();

                    state.entropy_device = Some(VirtioDeviceState {
                        device_id: rng_dev.id().to_string(),
                        device_state,
                        transport_state,
                    })
                }
                VirtioDeviceType::Pmem => {
                    let pmem_dev = locked_virtio_dev
                        .as_mut_any()
                        .downcast_mut::<Pmem>()
                        .unwrap();
                    let device_state = pmem_dev.save();
                    state.pmem_devices.push(VirtioDeviceState {
                        device_id: pmem_dev.config.id.clone(),
                        device_state,
                        transport_state,
                    });
                }
                VirtioDeviceType::Mem => {
                    let mem_dev = locked_virtio_dev
                        .as_mut_any()
                        .downcast_mut::<VirtioMem>()
                        .unwrap();
                    let device_state = mem_dev.save();

                    state.memory_device = Some(VirtioDeviceState {
                        device_id: mem_dev.id().to_string(),
                        device_state,
                        transport_state,
                    })
                }
            }
        }

        let root_ports = self
            .pci_segment
            .root_ports
            .iter()
            .map(|port| port.lock().expect("Poisoned lock").state())
            .collect();

        PciDevicesState {
            virtio_devices: state,
            root_ports,
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        pci_state: &Self::State,
    ) -> Result<Self, Self::Error> {
        let mem = constructor_args.mem;
        let state = &pci_state.virtio_devices;
        let root_ports = pci_state.root_ports.len();
        let hotplug_ports = u8::try_from(root_ports)
            .ok()
            .filter(|ports| *ports <= MAX_PCIE_HOTPLUG_PORTS)
            .ok_or(PciManagerError::TooManyRootPorts(root_ports))?;

        let mut pci_devices = PciDevices::new(constructor_args.vm, hotplug_ports)?;

        // Root ports first: a device that was behind one is restored
        // onto that port's secondary bus, which has to exist by then.
        pci_devices
            .pci_segment
            .restore_root_ports(constructor_args.vm, &pci_state.root_ports)?;

        if let Some(balloon_state) = &state.balloon_device {
            let device = Arc::new(Mutex::new(Balloon::restore(
                BalloonConstructorArgs { mem: mem.clone() },
                &balloon_state.device_state,
            )?));

            constructor_args
                .vm_resources
                .balloon
                .set_device(device.clone());

            pci_devices.restore_pci_device(
                constructor_args.vm,
                device,
                &balloon_state.device_id,
                &balloon_state.transport_state,
                constructor_args.event_manager,
            )?
        }

        for block_state in &state.block_devices {
            let device = Arc::new(Mutex::new(Block::restore(
                BlockConstructorArgs { mem: mem.clone() },
                &block_state.device_state,
            )?));

            device
                .lock()
                .expect("Poisoned lock")
                .spawn_worker(constructor_args.blk_worker_filter.clone())?;

            constructor_args
                .vm_resources
                .block
                .add_virtio_device(device.clone());

            pci_devices.restore_pci_device(
                constructor_args.vm,
                device,
                &block_state.device_id,
                &block_state.transport_state,
                constructor_args.event_manager,
            )?
        }

        // Initialize MMDS if MMDS state is included.
        if let Some(mmds) = &state.mmds {
            constructor_args.vm_resources.set_mmds_basic_config(
                mmds.version,
                mmds.imds_compat,
                constructor_args.instance_id,
            )?;
        } else if state
            .net_devices
            .iter()
            .any(|dev| dev.device_state.mmds_ns.is_some())
        {
            // If there's at least one network device having an mmds_ns, it means
            // that we are restoring from a version that did not persist the `MmdsVersionState`.
            // Init with the default.
            constructor_args.vm_resources.mmds_or_default()?;
        }

        for net_state in &state.net_devices {
            let device = Arc::new(Mutex::new(Net::restore(
                NetConstructorArgs {
                    mem: mem.clone(),
                    mmds: constructor_args
                        .vm_resources
                        .mmds
                        .as_ref()
                        // Clone the Arc reference.
                        .cloned(),
                },
                &net_state.device_state,
            )?));

            constructor_args
                .vm_resources
                .net_builder
                .add_device(device.clone());

            pci_devices.restore_pci_device(
                constructor_args.vm,
                device,
                &net_state.device_id,
                &net_state.transport_state,
                constructor_args.event_manager,
            )?
        }

        if let Some(vsock_state) = &state.vsock_device {
            let ctor_args = VsockUdsConstructorArgs {
                cid: vsock_state.device_state.frontend.cid,
            };
            let backend = VsockUnixBackend::restore(ctor_args, &vsock_state.device_state.backend)?;
            let device = Arc::new(Mutex::new(Vsock::restore(
                VsockConstructorArgs {
                    mem: mem.clone(),
                    backend,
                },
                &vsock_state.device_state.frontend,
            )?));

            constructor_args
                .vm_resources
                .vsock
                .set_device(device.clone());

            pci_devices.restore_pci_device(
                constructor_args.vm,
                device,
                &vsock_state.device_id,
                &vsock_state.transport_state,
                constructor_args.event_manager,
            )?
        }

        if let Some(entropy_state) = &state.entropy_device {
            let ctor_args = EntropyConstructorArgs { mem: mem.clone() };

            let device = Arc::new(Mutex::new(Entropy::restore(
                ctor_args,
                &entropy_state.device_state,
            )?));

            constructor_args
                .vm_resources
                .entropy
                .set_device(device.clone());

            pci_devices.restore_pci_device(
                constructor_args.vm,
                device,
                &entropy_state.device_id,
                &entropy_state.transport_state,
                constructor_args.event_manager,
            )?
        }

        for pmem_state in &state.pmem_devices {
            let device = Arc::new(Mutex::new(Pmem::restore(
                PmemConstructorArgs {
                    mem,
                    vm: constructor_args.vm.clone(),
                },
                &pmem_state.device_state,
            )?));

            constructor_args
                .vm_resources
                .pmem
                .configs
                .push(pmem_state.device_state.config.clone());

            pci_devices.restore_pci_device(
                constructor_args.vm,
                device,
                &pmem_state.device_id,
                &pmem_state.transport_state,
                constructor_args.event_manager,
            )?
        }

        if let Some(memory_device) = &state.memory_device {
            let ctor_args = VirtioMemConstructorArgs::new(Arc::clone(constructor_args.vm));
            let device = VirtioMem::restore(ctor_args, &memory_device.device_state)?;

            constructor_args.vm_resources.memory_hotplug = Some(MemoryHotplugConfig {
                total_size_mib: device.total_size_mib(),
                block_size_mib: device.block_size_mib(),
                slot_size_mib: device.slot_size_mib(),
            });

            let arcd_device = Arc::new(Mutex::new(device));
            pci_devices.restore_pci_device(
                constructor_args.vm,
                arcd_device,
                &memory_device.device_id,
                &memory_device.transport_state,
                constructor_args.event_manager,
            )?
        }

        // After PCI devices are restored, we must set up the GSI routes (one KVM_SET_GSI_ROUTING
        // call for all vectors), and enable all unmasked vectors (one kvm_irqfd call per
        // vector). Ordering: routing must be set before IRQFDs to avoid kernel panics on
        // older AMD/SVM hosts (see kernel commit a80ced6ea514).
        if !pci_devices.virtio_devices.is_empty() || !pci_state.root_ports.is_empty() {
            constructor_args
                .vm
                .set_gsi_routes()
                .map_err(PciManagerError::from)?;

            for pci_device in pci_devices.virtio_devices.values() {
                let dev = pci_device.lock().expect("Poisoned lock");
                dev.enable_unmasked_vectors()
                    .map_err(PciManagerError::from)?;
            }
            pci_devices.pci_segment.enable_rp_unmasked_vectors()?;
        }

        Ok(pci_devices)
    }
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::builder::tests::*;
    use crate::device_manager;
    use crate::device_manager::tests::{make_hotplug_block_cfg, pci_devices};
    use crate::device_manager::{HotplugDeviceConfig, VirtioDevices};
    use crate::devices::virtio::block::CacheType;
    use crate::mmds::data_store::MmdsVersion;
    use crate::pci::PciDevice;
    use crate::pci::pcie_cap::{
        PCI_EXP_SLTCTL, PCI_EXP_SLTCTL_HPIE, PCI_EXP_SLTCTL_PDCE, PCI_EXP_SLTSTA_PDC,
    };
    use crate::resources::VmmConfig;
    use crate::vmm_config::balloon::BalloonDeviceConfig;
    use crate::vmm_config::entropy::EntropyDeviceConfig;
    use crate::vmm_config::memory_hotplug::MemoryHotplugConfig;
    use crate::vmm_config::net::NetworkInterfaceConfig;
    use crate::vmm_config::pmem::PmemConfig;
    use crate::vmm_config::vsock::VsockDeviceConfig;
    use crate::vstate::resources::ResourceAllocator;

    #[test]
    fn test_static_pci_graph_does_not_add_hotplug_ports() {
        use crate::vmm_config::pci_topology::{
            PciAddress, PciBridgeConfig, PciBridgeKind, PciRootConfig, PciTopologyConfig,
        };

        for (base, size) in [
            (crate::arch::MMIO64_MEM_START, crate::arch::MMIO64_MEM_SIZE),
            (16 << 30, 8 << 30),
            (2 << 40, 2 << 40),
        ] {
            let vmm = default_vmm();
            let vm = vmm.vm.as_kvm().unwrap().clone();
            *vm.resource_allocator() = ResourceAllocator::with_mmio64_range(base, size).unwrap();
            let mut pci = PciDevices::new(&vm, 0).unwrap();
            let topology = PciTopologyConfig {
                roots: vec![
                    PciRootConfig {
                        bus: 0,
                        last_bus: 2,
                        numa_node: None,
                    },
                    PciRootConfig {
                        bus: 25,
                        last_bus: 25,
                        numa_node: None,
                    },
                ],
                bridges: vec![PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 0,
                        device: 13,
                        function: 0,
                    },
                    secondary_bus: 2,
                    subordinate_bus: 2,
                    kind: PciBridgeKind::RootPort,
                }],
            };
            pci.configure_static_topology(&topology, &[]).unwrap();
            pci.attach_vfio_devices(&vm, &[]).unwrap();
            pci.attach_root_ports(&vm).unwrap();

            assert!(pci.pci_segment.root_ports.is_empty());
            assert_eq!(pci.pci_segment.next_device_sbdf().unwrap().device(), 1);
            let root = pci.pci_segment.pci_buses.root_bus().lock().unwrap();
            let mut bridge = root.get_device(13).unwrap().lock().unwrap();
            assert_eq!(bridge.read_config_register(6), 0x0002_0200);
            assert!(pci.pci_segment.pci_buses.get(25).is_some());
            assert!(pci.pci_segment.pci_buses.get(1).is_none());
            assert_eq!(pci.pci_segment.start_of_mem64_area, base);
            assert_eq!(pci.pci_segment.end_of_mem64_area, base + size - 1);
        }
    }

    #[test]
    fn test_device_manager_persistence() {
        // These need to survive so the restored blocks find them.
        let _block_files;
        let _pmem_files;
        let mut tmp_sock_file = TempFile::new().unwrap();
        tmp_sock_file.remove().unwrap();

        let serialized_data;
        let saved_allocator;
        // Set up a vmm with one of each device, and get the serialized DeviceStates.
        {
            let mut event_manager = EventManager::new().expect("Unable to create EventManager");
            let mut vmm = default_vmm_with_pci();
            let mut cmdline = default_kernel_cmdline();

            // Add a balloon device.
            let balloon_cfg = BalloonDeviceConfig {
                amount_mib: 123,
                deflate_on_oom: false,
                stats_polling_interval_s: 1,
                free_page_hinting: false,
                free_page_reporting: false,
            };
            insert_balloon_device(&mut vmm, &mut cmdline, &mut event_manager, balloon_cfg);
            // Add a block device.
            let drive_id = String::from("root");
            let block_configs = vec![CustomBlockConfig::new(
                drive_id,
                true,
                None,
                true,
                CacheType::Unsafe,
            )];
            _block_files =
                insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);
            // Add a net device.
            let network_interface = NetworkInterfaceConfig {
                iface_id: String::from("netif"),
                host_dev_name: String::from("hostname"),
                guest_mac: None,
                mtu: None,
                rx_rate_limiter: None,
                tx_rate_limiter: None,
                dmb_size_mib: None,
            };
            insert_net_device_with_mmds(
                &mut vmm,
                &mut cmdline,
                &mut event_manager,
                network_interface,
                MmdsVersion::V2,
            );
            // Add a vsock device.
            let vsock_dev_id = "vsock";
            let vsock_config = VsockDeviceConfig {
                vsock_id: Some(vsock_dev_id.to_string()),
                guest_cid: 3,
                uds_path: tmp_sock_file.as_path().to_str().unwrap().to_string(),
                dmb_size_mib: None,
            };
            insert_vsock_device(&mut vmm, &mut cmdline, &mut event_manager, vsock_config);
            // Add an entropy device.
            let entropy_config = EntropyDeviceConfig::default();
            insert_entropy_device(&mut vmm, &mut cmdline, &mut event_manager, entropy_config);
            // Add a pmem device.
            let pmem_id = String::from("pmem");
            let pmem_configs = vec![PmemConfig {
                id: pmem_id,
                path_on_host: "".into(),
                root_device: true,
                read_only: true,
                ..Default::default()
            }];
            _pmem_files =
                insert_pmem_devices(&mut vmm, &mut cmdline, &mut event_manager, pmem_configs);

            let memory_hotplug_config = MemoryHotplugConfig {
                total_size_mib: 1024,
                block_size_mib: 2,
                slot_size_mib: 128,
            };
            insert_virtio_mem_device(
                &mut vmm,
                &mut cmdline,
                &mut event_manager,
                memory_hotplug_config,
            );

            let device_state = vmm.device_manager.save();
            serialized_data = bitcode::serialize(&device_state).unwrap();
            saved_allocator = vmm.vm.as_kvm().unwrap().resource_allocator().save()
        }

        tmp_sock_file.remove().unwrap();

        let mut event_manager = EventManager::new().expect("Unable to create EventManager");
        // Keep in mind we are re-creating here an empty DeviceManager. Restoring later on
        // will create a new PciDevices manager different from vmm's virtio devices. We're
        // doing this to avoid restoring the whole Vmm, since what we really need from Vmm is the
        // KvmVm object and calling default_vmm() is the easiest way to create one.
        let vmm = default_vmm();
        // Restore the source allocator's state so the restored devices' GSIs match what their
        // `MsixVectorGroup::Drop` will try to free at end-of-test.
        *vmm.vm.as_kvm().unwrap().resource_allocator() =
            ResourceAllocator::restore((), &saved_allocator).unwrap();

        let device_manager_state: device_manager::DevicesState =
            bitcode::deserialize(&serialized_data).unwrap();
        let device_manager::VirtioDevicesState::Pci(pci_state) = &device_manager_state.virtio_state
        else {
            panic!("expected PCI virtio device state");
        };
        let vm_resources = &mut VmResources::default();
        let kvm_vm = vmm.vm.as_kvm().unwrap().clone();
        let restore_args = PciDevicesConstructorArgs {
            vm: &kvm_vm,
            mem: kvm_vm.guest_memory(),
            vm_resources,
            instance_id: "microvm-id",
            event_manager: &mut event_manager,
            blk_worker_filter: None,
        };
        let _restored_dev_manager = PciDevices::restore(restore_args, pci_state).unwrap();

        let expected_vm_resources = format!(
            r#"{{
  "balloon": {{
    "amount_mib": 123,
    "deflate_on_oom": false,
    "stats_polling_interval_s": 1,
    "free_page_hinting": false,
    "free_page_reporting": false
  }},
  "drives": [
    {{
      "drive_id": "root",
      "partuuid": null,
      "is_root_device": true,
      "cache_type": "Unsafe",
      "is_read_only": true,
      "discard": false,
      "num_queues": null,
      "path_on_host": "{}",
      "rate_limiter": null,
      "io_engine": "Sync",
      "blk_size": 512,
      "topology": {{
        "physical_block_exp": 0,
        "alignment_offset": 0,
        "min_io_size": 0,
        "opt_io_size": 128
      }},
      "socket": null
    }}
  ],
  "boot-source": {{
    "kernel_image_path": "",
    "initrd_path": null,
    "boot_args": null
  }},
  "cpu-config": null,
  "logger": null,
  "machine-config": {{
    "vcpu_count": 1,
    "mem_size_mib": 128,
    "smt": false,
    "track_dirty_pages": false,
    "huge_pages": "None",
    "pcie_hotplug_ports": 0
  }},
  "metrics": null,
  "mmds-config": {{
    "version": "V2",
    "network_interfaces": [
      "netif"
    ],
    "ipv4_address": "169.254.169.254",
    "imds_compat": false
  }},
  "network-interfaces": [
    {{
      "iface_id": "netif",
      "host_dev_name": "hostname",
      "guest_mac": null,
      "mtu": null,
      "rx_rate_limiter": null,
      "tx_rate_limiter": null
    }}
  ],
  "vsock": {{
    "guest_cid": 3,
    "uds_path": "{}"
  }},
  "entropy": {{
    "rate_limiter": null
  }},
  "pmem": [
    {{
      "id": "pmem",
      "path_on_host": "{}",
      "root_device": true,
      "read_only": true,
      "rate_limiter": null
    }}
  ],
  "vfio": [],
  "memory-hotplug": {{
    "total_size_mib": 1024,
    "block_size_mib": 2,
    "slot_size_mib": 128
  }}
}}"#,
            _block_files.last().unwrap().as_path().to_str().unwrap(),
            tmp_sock_file.as_path().to_str().unwrap(),
            _pmem_files.last().unwrap().as_path().to_str().unwrap(),
        );

        assert_eq!(
            vm_resources
                .mmds
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .version(),
            MmdsVersion::V2
        );
        assert_eq!(
            pci_state.virtio_devices.mmds.as_ref().unwrap().version,
            MmdsVersion::V2
        );
        assert_eq!(
            expected_vm_resources,
            serde_json::to_string_pretty(&VmmConfig::from(&*vm_resources)).unwrap()
        );
    }

    #[test]
    fn test_root_port_state_round_trip() {
        let mut event_manager = EventManager::new().unwrap();
        let mut cmdline = default_kernel_cmdline();
        let mut vmm = default_vmm_with_pci_ports(2);

        let _block_files = insert_block_devices(
            &mut vmm,
            &mut cmdline,
            &mut event_manager,
            vec![CustomBlockConfig::new(
                "root".to_string(),
                true,
                None,
                true,
                CacheType::Unsafe,
            )],
        );
        vmm.device_manager
            .attach_root_ports(vmm.vm.as_kvm().unwrap())
            .unwrap();

        // Arm the port's interrupts the way a guest driver would, then give it
        // a device so the slot registers are not all zero.
        const SLOT_CONTROL: u16 = PCI_EXP_SLTCTL_HPIE | PCI_EXP_SLTCTL_PDCE;
        const SLOT_STATUS: u16 = PCI_EXP_SLTSTA_PDC;

        let segment = &pci_devices(&vmm.device_manager).pci_segment;
        let mut port = segment.root_ports[0].lock().unwrap();
        let slot_reg = (port.state().pcie_cap_offset + PCI_EXP_SLTCTL) / 4;
        port.write_config_register(slot_reg, 0, &SLOT_CONTROL.to_le_bytes());
        port.plug();
        assert_ne!(port.state().slot_status & SLOT_STATUS, 0);
        drop(port);

        let expected = segment
            .root_ports
            .iter()
            .map(|port| port.lock().unwrap().state())
            .collect::<Vec<_>>();
        // Slot 1 went to the block device, so the ports are at 2 and 3.
        assert_eq!(expected[0].sbdf.device(), 2);
        assert_eq!(expected[1].sbdf.device(), 3);

        let serialized_data = bitcode::serialize(&vmm.device_manager.save()).unwrap();
        let saved_allocator = vmm.vm.as_kvm().unwrap().resource_allocator().save();
        drop(vmm);

        let mut restored_vmm = default_vmm();
        *restored_vmm.vm.as_kvm().unwrap().resource_allocator() =
            ResourceAllocator::restore((), &saved_allocator).unwrap();

        let device_manager_state: device_manager::DevicesState =
            bitcode::deserialize(&serialized_data).unwrap();
        let device_manager::VirtioDevicesState::Pci(pci_state) = &device_manager_state.virtio_state
        else {
            panic!("expected PCI virtio device state");
        };

        let kvm_vm = restored_vmm.vm.as_kvm().unwrap().clone();
        let restored = PciDevices::restore(
            PciDevicesConstructorArgs {
                vm: &kvm_vm,
                mem: kvm_vm.guest_memory(),
                vm_resources: &mut VmResources::default(),
                instance_id: "microvm-id",
                event_manager: &mut event_manager,
                blk_worker_filter: None,
            },
            pci_state,
        )
        .unwrap();

        // The ports come back exactly as they were, so the guest's view of them
        // still holds.
        let actual = restored
            .pci_segment
            .root_ports
            .iter()
            .map(|port| port.lock().unwrap().state())
            .collect::<Vec<_>>();
        assert_eq!(actual.len(), 2);
        assert_eq!(
            bitcode::serialize(&expected).unwrap(),
            bitcode::serialize(&actual).unwrap()
        );
        assert_eq!(actual[0].slot_control, SLOT_CONTROL);
        assert_eq!(actual[0].slot_status & SLOT_STATUS, SLOT_STATUS);
        assert_ne!(actual[0].link_status, 0, "port 0 had a device in it");
        assert_eq!(actual[1].link_status, 0, "port 1 was empty");

        // And a hot-plug into a restored port still works, which is what the
        // MSI-X restore is for.
        restored_vmm.device_manager.virtio_devices = VirtioDevices::Pci(Box::new(restored));
        let f = TempFile::new().unwrap();
        let cfg = HotplugDeviceConfig::Block(make_hotplug_block_cfg("hp0", &f, false));
        restored_vmm
            .hotplug_device(cfg, &mut event_manager)
            .unwrap();
    }

    #[test]
    fn test_place_vfio_bars() {
        use crate::arch::{
            MEM_32BIT_DEVICES_SIZE, MEM_32BIT_DEVICES_START, MEM_64BIT_DEVICES_SIZE,
            MEM_64BIT_DEVICES_START,
        };
        use crate::devices::vfio::pci::{BarRequirement, BarSlot, BarWindow};

        let mut allocator = ResourceAllocator::new();
        // A virtio-pci BAR, allocated from the bottom of the 32-bit window as
        // `VirtioPciDevice::allocate_bars` does.
        let virtio = allocator
            .mmio32_memory
            .allocate(
                CAPABILITY_BAR_SIZE,
                CAPABILITY_BAR_SIZE,
                vm_allocator::AllocPolicy::FirstMatch,
            )
            .unwrap()
            .start();
        let requirement = |slot, size, window| BarRequirement { slot, size, window };
        let half_window = MEM_64BIT_DEVICES_SIZE / 2;
        let requirements = vec![
            (0, requirement(BarSlot::Bar(0), 16 << 20, BarWindow::Mmio32)),
            (
                0,
                requirement(BarSlot::Bar(1), 256 << 20, BarWindow::Mmio64),
            ),
            (0, requirement(BarSlot::Bar(3), 32 << 20, BarWindow::Mmio64)),
            (0, requirement(BarSlot::Rom, 512 << 10, BarWindow::Mmio32)),
            (1, requirement(BarSlot::Bar(0), 16 << 20, BarWindow::Mmio32)),
            (
                1,
                requirement(BarSlot::Bar(1), half_window, BarWindow::Mmio64),
            ),
        ];
        let placements = place_vfio_bars(&mut allocator, requirements.clone()).unwrap();

        // Largest first; equal sizes keep their order.
        let order: Vec<(usize, BarSlot)> = placements
            .iter()
            .map(|&(device, slot, _)| (device, slot))
            .collect();
        assert_eq!(
            order,
            vec![
                (1, BarSlot::Bar(1)),
                (0, BarSlot::Bar(1)),
                (0, BarSlot::Bar(3)),
                (0, BarSlot::Bar(0)),
                (1, BarSlot::Bar(0)),
                (0, BarSlot::Rom),
            ]
        );
        // The largest BAR takes the top half of the 64-bit window, the next ones are packed right
        // below it.
        let top64 = MEM_64BIT_DEVICES_START + MEM_64BIT_DEVICES_SIZE;
        assert_eq!(placements[0].2, top64 - half_window);
        assert_eq!(placements[1].2, top64 - half_window - (256 << 20));
        assert_eq!(
            placements[2].2,
            top64 - half_window - (256 << 20) - (32 << 20)
        );
        // The BARs of the 32-bit window are placed from its top, clear of the virtio-pci BAR.
        for &(_, _, addr) in &placements[3..] {
            assert!(addr >= virtio + CAPABILITY_BAR_SIZE);
        }
        // Every BAR is naturally aligned and inside its window, and no two BARs overlap.
        for (&(device, slot, addr), i) in placements.iter().zip(0..) {
            let (_, requirement) = requirements
                .iter()
                .find(|(d, r)| *d == device && r.slot == slot)
                .unwrap();
            assert_eq!(addr % requirement.size, 0);
            let (start, size) = match requirement.window {
                BarWindow::Mmio32 => (MEM_32BIT_DEVICES_START, MEM_32BIT_DEVICES_SIZE),
                BarWindow::Mmio64 => (MEM_64BIT_DEVICES_START, MEM_64BIT_DEVICES_SIZE),
            };
            assert!(addr >= start && addr + requirement.size <= start + size);
            for &(other_device, other_slot, other_addr) in &placements[i + 1..] {
                let (_, other) = requirements
                    .iter()
                    .find(|(d, r)| *d == other_device && r.slot == other_slot)
                    .unwrap();
                assert!(addr + requirement.size <= other_addr || other_addr + other.size <= addr);
            }
        }

        // A BAR as large as the whole 64-bit window no longer fits next to the BARs placed in it.
        let whole_window = vec![(
            0,
            requirement(BarSlot::Bar(0), MEM_64BIT_DEVICES_SIZE, BarWindow::Mmio64),
        )];
        place_vfio_bars(&mut ResourceAllocator::new(), whole_window.clone()).unwrap();
        place_vfio_bars(&mut allocator, whole_window).unwrap_err();
    }

    #[test]
    fn test_intx_gsis() {
        use crate::arch::GSI_LEGACY_END;

        let mut allocator = ResourceAllocator::new();
        let mut gsis = IntxGsis::default();
        // Each device gets a GSI of its own from INTX_GSI_START while some are free...
        let own: Vec<u32> = (INTX_GSI_START..=GSI_LEGACY_END)
            .map(|_| gsis.next(&mut allocator).unwrap())
            .collect();
        assert_eq!(own, (INTX_GSI_START..=GSI_LEGACY_END).collect::<Vec<_>>());
        // ...then they share the GSIs, in turn.
        let shared: Vec<u32> = (0..own.len() + 1)
            .map(|_| gsis.next(&mut allocator).unwrap())
            .collect();
        assert_eq!(shared[..own.len()], own);
        assert_eq!(shared[own.len()], own[0]);
        // With no GSI at all, routing fails.
        let mut allocator = ResourceAllocator::new();
        while allocator.allocate_gsi_legacy(1).is_ok() {}
        IntxGsis::default().next(&mut allocator).unwrap_err();
    }

    #[test]
    fn test_add_vfio_devices() {
        use crate::devices::vfio::pci::tests::{mock_vfio_device, setup_vm_with_irqchip};

        let vm = Arc::new(setup_vm_with_irqchip());
        let mut pci_devices = PciDevices::new(&vm, 0).unwrap();
        let first = pci_devices.pci_segment.next_device_sbdf().unwrap().device();

        // Explicit flat addresses preserve INTA routing and resource allocation.
        let ids = ["gpu0", "audio0"];
        use crate::vmm_config::pci_topology::*;
        let topology = PciTopologyConfig {
            roots: vec![PciRootConfig {
                bus: 0,
                last_bus: 0,
                numa_node: None,
            }],
            bridges: vec![],
        };
        let configs: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| crate::vmm_config::vfio::VfioDeviceConfig {
                id: id.to_string(),
                path: "/host/device".into(),
                pci_address: Some(PciAddress {
                    bus: 0,
                    device: first + u8::try_from(i).unwrap(),
                    function: 0,
                }),
            })
            .collect();
        pci_devices
            .configure_static_topology(&topology, &configs)
            .unwrap();
        pci_devices
            .add_vfio_devices(
                &vm,
                ids.map(|id| {
                    let vm = vm.clone();
                    move |sbdf| Ok(mock_vfio_device(id, sbdf, vm))
                }),
            )
            .unwrap();
        let slot_of = |id: &str| pci_devices.vfio_devices[id].lock().unwrap().sbdf().device();
        assert_eq!(ids.map(&slot_of), [first, first + 1]);
        {
            let bus = pci_devices.pci_segment.pci_buses.root_bus().lock().unwrap();
            for id in ids {
                assert!(bus.get_device(slot_of(id)).is_some());
            }
        }
        assert_eq!(
            pci_devices.pci_segment.next_device_sbdf().unwrap().device(),
            first + 2
        );

        // Each INTA pin is routed to a GSI of its own, for the slot of its device.
        let routes = pci_devices.intx_routes();
        let slots: Vec<u8> = routes.iter().map(|&(slot, _)| slot).collect();
        assert_eq!(slots, [first, first + 1]);
        assert_ne!(routes[0].1, routes[1].1);
        #[cfg(target_arch = "x86_64")]
        for (slot, gsi) in routes {
            let pci_irq_slot = pci_devices.pci_segment.pci_irq_slots[usize::from(slot)];
            assert_eq!(u32::from(pci_irq_slot), gsi);
        }

        // The BARs of the devices do not overlap.
        let mut ranges: Vec<(u64, u64)> = pci_devices
            .vfio_devices
            .values()
            .flat_map(|device| device.lock().unwrap().mmio_ranges())
            .collect();
        ranges.sort_unstable();
        for pair in ranges.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{pair:x?}");
        }
    }

    #[test]
    fn test_virtio_attach_without_mmio32_space() {
        use crate::devices::virtio::rng::Entropy;
        use crate::devices::virtio::transport::pci::device::CAPABILITY_BAR_SIZE;
        use crate::rate_limiter::RateLimiter;

        let mut vmm = default_vmm_with_pci();
        let vm = vmm.vm.as_kvm().unwrap().clone();
        // Passthrough BARs can take all the room left in the 32-bit MMIO window.
        {
            let mut allocator = vm.resource_allocator();
            while allocator
                .mmio32_memory
                .allocate(
                    CAPABILITY_BAR_SIZE,
                    CAPABILITY_BAR_SIZE,
                    vm_allocator::AllocPolicy::FirstMatch,
                )
                .is_ok()
            {}
        }

        // A virtio-pci device then fails to attach, without bringing the VMM down.
        let entropy = Arc::new(Mutex::new(Entropy::new(RateLimiter::default()).unwrap()));
        let mut event_manager = EventManager::new().unwrap();
        let err = device_manager::tests::pci_devices_mut(&mut vmm.device_manager)
            .attach_pci_virtio_device(
                &vm,
                "rng".to_string(),
                entropy,
                &mut event_manager,
                PciPlacement::PrimaryBus,
            )
            .unwrap_err();
        assert!(matches!(err, PciManagerError::VirtioPciDevice(_)), "{err}");
    }
}

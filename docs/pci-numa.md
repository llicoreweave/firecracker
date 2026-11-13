# PCI passthrough with NUMA locality

This implementation extends the GPU-capable VFIO branch at
`5aa6bd2619d4769c824eb6d65a49bee476cb8a32`. It uses the branch's `vfio`
configuration API. It is a local implementation of HAVOCK-2250, building on
the October 21 topology and DMA-BUF prototype.

Start Firecracker with `--enable-pci --config-file <file>`. The caller owns
all topology policy. Firecracker consumes explicit guest PCI roots, bridges,
port types, bus registers, endpoint BDFs and root NUMA affinity. It never reads
host PCI ancestry or locality to choose any of these settings.

```json
{
  "machine-config": {"vcpu_count": 2, "mem_size_mib": 2048},
  "numa": {
    "nodes": [
      {"host_node": 0, "vcpus": [0], "host_cpus": [0], "memory_mib": 1024},
      {"host_node": 1, "vcpus": [1], "host_cpus": [1], "memory_mib": 1024}
    ],
    "distances": [[10, 21], [21, 10]]
  },
  "pci-topology": {
    "roots": [
      {"bus": 0, "last_bus": 9, "numa_node": 0},
      {"bus": 20, "last_bus": 29, "numa_node": 1}
    ],
    "bridges": [
      {"pci_address": {"bus": 0, "device": 13, "function": 0}, "secondary_bus": 7, "subordinate_bus": 7, "type": "root-port"},
      {"pci_address": {"bus": 20, "device": 13, "function": 0}, "secondary_bus": 25, "subordinate_bus": 25, "type": "root-port"}
    ]
  },
  "vfio": [
    {"id": "gpu0", "path": "/sys/bus/pci/devices/0000:19:00.0", "pci_address": {"bus": 7, "device": 0, "function": 0}},
    {"id": "gpu1", "path": "/sys/bus/pci/devices/0000:83:00.0", "pci_address": {"bus": 25, "device": 0, "function": 0}}
  ]
}
```

Merge this fragment with the VM's boot source, drives and other configuration.
The generator must supply actual host CPU IDs and distances and allocate devices
exclusively, already bound to `vfio-pci`. Guest bus numbers can differ from host
BDFs. In this example the guest endpoints are `0000:07:00.0` and `0000:19:00.0`.
Sparse bus ranges and multiple PCI roots with the same NUMA affinity are allowed;
reordering roots or changing affinity never renumbers any device.

`roots[].numa_node` is the guest domain index in `numa.nodes`, not `host_node`.
The VMM emits it directly as the ACPI PCI root's `_PXM`. When omitted, that root
has no `_PXM`. Root identity and `_BBN` come from its configured bus number.
ACPI SRAT/SLIT describe the independently configured CPU/memory domains. Linux
derives subordinate endpoint locality through its PCI root bridge; a root-port
`_PXM` alone does not establish endpoint locality on x86 Linux.

Every bridge explicitly supplies its guest address (including primary bus),
secondary bus, subordinate bus and PCIe port type. No missing bridge or endpoint
address is generated. Invalid addresses, duplicate BDFs, cycles, disconnected
buses, overlapping roots/windows, out-of-range domains and exhausted ECAM ranges
fail validation before VFIO devices are opened. The old `pci_path` and
`pci_root_slots` fields are no longer accepted. A flat VFIO configuration also
supplies an explicit root and endpoint addresses, with `bridges: []`.

The VMM still probes device BAR requirements and allocates MMIO resources within
the configured graph, including disjoint ACPI root windows and 1-MiB bridge
window granularity. This resource allocation does not choose PCI connections,
bus numbers, port types or locality. The x86 MMIO64 aperture spans 2 TiB starting
at 2 TiB; exhaustion fails startup. CF8/CFC and ECAM both decode secondary buses.

For jailed VMs, pass `--numa-node <host-id>` for each configured host node,
alongside assigned `--vfio-device` arguments. Kata emits these automatically.
The jailer supplies only selected CPU membership for CPU-affinity validation;
it no longer copies device `numa_node` files. Host sysfs stays outside the jail.

RAM ranges split at NUMA boundaries without counting PCI holes as RAM. Each
mapping receives `MPOL_BIND` before first touch and VFIO pinning. Each vCPU
thread pins to its corresponding host CPU before entering KVM. CPU locality,
effective affinity, RAM totals, and the explicit PCI graph are
validated. Host CPU pinning does not reserve those CPUs exclusively.

The static passthrough mode also retains IOMMUFD/VFIO DMA-BUF BAR export/import
for peer DMA, without disabling the host IOMMU or ACS. This requires compatible
host VFIO/IOMMUFD support and guest GPU/RDMA drivers. A successful VM boot alone
does not establish successful GPU peer DMA.

## Kata integration

The accompanying Kata checkout enables this configuration with `enable_numa`
and `cold_plug_vfio = "switch-port"`. An optional `numa_mapping` fixes domain
order; otherwise its runtime planner selects permitted host domains from the
allocated devices and CPU capacity. The planner reads device ancestry and
locality from sysfs, respects effective and pod CPU/memory-node constraints,
apportions guest CPU/RAM, and uses the host NUMA distance matrix. It writes the
static configuration before starting the VMM and keeps the guest agent's PCI
paths and NUMA translation synchronized with it. The guest agent parses the
selected `pciDDDD:BB` root when resolving device paths. See the Kata checkout's
`docs/how-to/firecracker-numa-pci.md` for configuration and resource semantics.

## Limits

- x86_64 cold boot; up to 64 vCPUs, 64 guest buses and host node IDs below 64.
- Guest PCI functions use function zero. At most seven bridge ancestors are
  supported per endpoint; slot zero on bus zero is reserved for the host bridge.
- No bridge bus renumbering, BAR relocation, device hotplug, memory hotplug,
  ballooning or NUMA snapshots. Unsupported combinations fail validation.
- Kata's existing PCI path prefix type limits root bus prefixes to 00–1f;
  an unrepresentable root fails configuration before starting the VM.
- Existing configurations that used inferred VFIO addresses must be regenerated
  with explicit `pci-topology` and `vfio[].pci_address`.

All build dependencies and tests run in Linux containers. The hardware-only
`numa::tests::test_numa_host_memory_binding` test explicitly verifies both
memory policy and resident pages on a two-node host.

The shared wire fixture `docs/fixtures/kata-pci-topology.json` is generated by
Kata and consumed by the VMM's Rust tests. Sparse numbering, reordered bridge
entries, shared root affinity, malformed graphs and BAR containment are covered
separately from host hardware checks.

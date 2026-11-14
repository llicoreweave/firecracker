# NUMA configuration

The optional `numa` configuration-file section explicitly lists guest domains,
their vCPUs, host CPUs, host NUMA nodes, RAM sizes and the SLIT distance matrix.
Guest proximity domains are numbered by array index. Host node IDs need not be
contiguous. KVM supports up to 64 vCPUs and this mode requires x86_64 and
`--enable-pci`.

```json
{
  "numa": {
    "nodes": [
      {
        "host_node": 0,
        "vcpus": [
          0
        ],
        "host_cpus": [
          0
        ],
        "memory_mib": 256
      },
      {
        "host_node": 1,
        "vcpus": [
          1
        ],
        "host_cpus": [
          1
        ],
        "memory_mib": 256
      }
    ],
    "distances": [
      [
        10,
        21
      ],
      [
        21,
        10
      ]
    ]
  }
}
```

CPU and RAM totals must match the machine configuration. CPU membership and
effective affinity are validated before startup. RAM receives `MPOL_BIND` before
first touch or VFIO pinning, and each vCPU pins to its supplied host CPU before
entering KVM. ACPI SRAT and SLIT describe the supplied guest layout. The caller
must select and reserve suitable host resources; CPU affinity alone does not
reserve a CPU.

For a jailed VM, pass `--numa-node <host-node>` for every selected host node.
The jailer copies only the CPU-membership metadata needed for validation.
Snapshots, ballooning, memory hotplug and memfd-backed RAM are unsupported in
this mode. Configurations without `numa` retain their existing behavior.

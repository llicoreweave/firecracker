# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Boot explicit PCI roots and NUMA domains without passthrough hardware."""

import json
import os
import platform
from pathlib import Path

import pytest

from framework.artifacts import pin_pci
from framework.kvm import default_boot_args
from framework.vm_backend import VM_BACKEND_KVM, pin_vm_backend

pytestmark = pin_vm_backend(VM_BACKEND_KVM)


def _host_nodes():
    """Select one permitted CPU on each available host NUMA node."""
    allowed = os.sched_getaffinity(0)
    nodes = []
    for directory in sorted(Path("/sys/devices/system/node").glob("node[0-9]*")):
        cpus = [
            int(cpu.name[3:])
            for cpu in directory.glob("cpu[0-9]*")
            if int(cpu.name[3:]) in allowed
        ]
        node = int(directory.name[4:])
        if cpus and node < 64:
            nodes.append((node, min(cpus)))
    return nodes


@pytest.mark.skipif(platform.machine() != "x86_64", reason="requires x86 ACPI")
@pin_pci(True)
@pytest.mark.parametrize("aperture", [None, {"base_gib": 16, "size_gib": 8}])
def test_explicit_pci_roots_and_numa(uvm, aperture):
    """Guest CPU and PCI locality follow the file's explicit proximity domains."""
    nodes = _host_nodes()[:2]
    if len(nodes) != 2:
        pytest.skip("requires two permitted host NUMA nodes")
    vm = uvm
    vm.jailer.numa_nodes = [node for node, _ in nodes]
    iface = vm.add_net_iface(api=False)
    config = {
        "boot-source": {
            "kernel_image_path": vm.create_jailed_resource(vm.kernel_file),
            "boot_args": default_boot_args(True),
        },
        "drives": [
            {
                "drive_id": "rootfs",
                "is_root_device": True,
                "is_read_only": True,
                "path_on_host": vm.create_jailed_resource(vm.rootfs_file),
            }
        ],
        "network-interfaces": [
            {
                "iface_id": iface.dev_name,
                "host_dev_name": iface.tap_name,
                "guest_mac": iface.guest_mac,
            }
        ],
        "machine-config": {"vcpu_count": 2, "mem_size_mib": 512},
        "numa": {
            "nodes": [
                {
                    "host_node": node,
                    "vcpus": [index],
                    "host_cpus": [cpu],
                    "memory_mib": 256,
                }
                for index, (node, cpu) in enumerate(nodes)
            ],
            "distances": [[10, 21], [21, 10]],
        },
        "pci-topology": {
            "roots": [
                {"bus": 0, "last_bus": 7, "numa_node": 1},
                {"bus": 20, "last_bus": 25, "numa_node": 0},
            ],
            "bridges": [
                {
                    "pci_address": {"bus": 20, "device": 13, "function": 0},
                    "secondary_bus": 25,
                    "subordinate_bus": 25,
                    "type": "root-port",
                }
            ],
        },
    }
    if aperture is not None:
        config["machine-config"]["pci_mmio64"] = aperture
    path = Path(vm.chroot()) / "topology.json"
    path.write_text(json.dumps(config), encoding="utf-8")
    vm.jailer.extra_args["config-file"] = path.name
    vm.spawn()
    vm.wait_for_ssh_up()
    for guest_node in range(2):
        assert vm.ssh.check_output(
            f"cat /sys/devices/system/node/node{guest_node}/cpulist"
        ).stdout.strip() == str(guest_node)
    assert (
        vm.ssh.check_output(
            "cat /sys/bus/pci/devices/0000:14:0d.0/numa_node"
        ).stdout.strip()
        == "0"
    )
    full_config = vm.api.vm_config.get().json()
    assert full_config["pci-topology"] == config["pci-topology"]
    assert full_config["numa"] == config["numa"]
    assert full_config["machine-config"].get("pci_mmio64") == aperture
    base = (aperture or {"base_gib": 256})["base_gib"] << 30
    size = (aperture or {"size_gib": 256})["size_gib"] << 30
    # Only root zero has BARs in this hardware-free configuration. Its ACPI
    # window must advertise the caller's aperture rather than the defaults.
    resources = vm.ssh.check_output("cat /proc/iomem").stdout
    assert any(
        line.strip() == f"{base:x}-{base + size - 1:x} : PCI Bus 0000:00"
        for line in resources.splitlines()
    )
    vm.pause()
    with pytest.raises(RuntimeError, match="NUMA"):
        vm.api.snapshot_create.put(mem_file_path="/mem", snapshot_path="/state")

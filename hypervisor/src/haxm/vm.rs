// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ffi::c_void;
use fnv::FnvHashMap;
use libc::{E2BIG, EEXIST, EFAULT, EINVAL, EIO, ENOENT, ENOSPC, ENOTSUP, EOVERFLOW};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::mem::ManuallyDrop;
use std::sync::Arc;

use crate::{
    host_phys_addr_bits, ClockState, Datamatch, DeviceKind, Hypervisor, IoEventAddress, MemSlot,
    VcpuX86_64, Vm, VmCap, VmX86_64,
};
use base::{
    errno_result, error, ioctl_with_mut_ref, ioctl_with_ref, warn, AsRawDescriptor, Error, Event,
    MappedRegion, MemoryMapping, MmapError, Protection, RawDescriptor, Result, SafeDescriptor,
};
use sync::Mutex;
use vm_memory::{GuestAddress, GuestMemory};
#[cfg(windows)]
use win_util::win32_wide_string;

use super::*;

/// A wrapper around creating and using a HAXM VM.
pub struct HaxmVm {
    haxm: Haxm,
    vm_id: u32,
    descriptor: SafeDescriptor,
    guest_mem: GuestMemory,
    mem_regions: Arc<Mutex<BTreeMap<MemSlot, (GuestAddress, Box<dyn MappedRegion>)>>>,
    /// A min heap of MemSlot numbers that were used and then removed and can now be re-used
    mem_slot_gaps: Arc<Mutex<BinaryHeap<Reverse<MemSlot>>>>,
    // HAXM's implementation of ioevents makes several assumptions about how crosvm uses ioevents:
    //   1. All ioevents are registered during device setup, and thus can be cloned when the vm
    //      is cloned instead of locked in an Arc<Mutex<>>. This will make handling ioevents in
    //      each vcpu thread easier because no locks will need to be acquired.
    //   2. All ioevents use Datamatch::AnyLength. We don't bother checking the datamatch, which
    //      will make this faster.
    //   3. We only ever register one eventfd to each address. This simplifies our data structure.
    ioevents: FnvHashMap<IoEventAddress, Event>,
}

impl HaxmVm {
    /// Constructs a new `HaxmVm` using the given `Haxm` instance.
    pub fn new(haxm: &Haxm, guest_mem: GuestMemory) -> Result<HaxmVm> {
        let mut vm_id: u32 = 0;
        // Safe because we know descriptor is a real haxm descriptor as this module is the only
        // one that can make Haxm objects.
        let ret = unsafe { ioctl_with_mut_ref(haxm, HAX_IOCTL_CREATE_VM(), &mut vm_id) };
        if ret != 0 {
            return errno_result();
        }

        // Haxm creates additional device paths when VMs are created
        let vm_descriptor = open_haxm_vm_device(USE_GHAXM.load(Ordering::Relaxed), vm_id)?;

        guest_mem.with_regions(|_, guest_addr, size, host_addr, _, _| {
            unsafe {
                // Safe because the guest regions are guaranteed not to overlap.
                set_user_memory_region(
                    &vm_descriptor,
                    false,
                    guest_addr.offset(),
                    size as u64,
                    MemoryRegionOp::Add(host_addr as *mut u8 as u64),
                )
            }
        })?;

        Ok(HaxmVm {
            vm_id,
            haxm: haxm.try_clone()?,
            descriptor: vm_descriptor,
            guest_mem,
            mem_regions: Arc::new(Mutex::new(BTreeMap::new())),
            mem_slot_gaps: Arc::new(Mutex::new(BinaryHeap::new())),
            ioevents: FnvHashMap::default(),
        })
    }

    pub fn check_raw_capability(&self, cap: u32) -> bool {
        let mut capability_info = hax_capabilityinfo::default();
        let ret =
            unsafe { ioctl_with_mut_ref(&self.haxm, HAX_IOCTL_CAPABILITY(), &mut capability_info) };

        if ret != 0 {
            return false;
        }

        return (cap & capability_info.winfo as u32) != 0;
    }

    pub fn register_log_file(&self, path: &str) -> Result<()> {
        // The IOCTL here is only avilable on internal fork of HAXM and only works on Windows.
        #[cfg(windows)]
        if USE_GHAXM.load(Ordering::Relaxed) {
            let mut log_file = hax_log_file::default();

            // Although it would be more efficient to do this check prior to allocating the log_file
            // struct, the code would be more complex and less maintainable. This is only ever called
            // once per-vm so the extra temporary memory and time shouldn't be a problem.
            if path.len() >= log_file.path.len() {
                return Err(Error::new(E2BIG));
            }

            let wstring = &win32_wide_string(path);
            log_file.path[..wstring.len()].clone_from_slice(wstring);

            let ret = unsafe { ioctl_with_ref(self, HAX_VM_IOCTL_REGISTER_LOG_FILE(), &log_file) };

            if ret != 0 {
                return errno_result();
            }
        }
        Ok(())
    }
}

impl AsRawDescriptor for HaxmVm {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.descriptor.as_raw_descriptor()
    }
}

enum MemoryRegionOp {
    // Map a memory region for the given host address.
    Add(u64),
    // Remove the memory region.
    Remove,
}

unsafe fn set_user_memory_region(
    descriptor: &SafeDescriptor,
    read_only: bool,
    guest_addr: u64,
    size: u64,
    op: MemoryRegionOp,
) -> Result<()> {
    let (va, flags) = match op {
        MemoryRegionOp::Add(va) => {
            let mut flags = HAX_RAM_INFO_STANDALONE;
            if read_only {
                flags |= HAX_RAM_INFO_ROM
            }
            (va, flags)
        }
        MemoryRegionOp::Remove => (0, HAX_RAM_INFO_INVALID),
    };
    let ram_info = hax_set_ram_info2 {
        pa_start: guest_addr,
        size,
        va,
        flags,
        ..Default::default()
    };

    let ret = ioctl_with_ref(descriptor, HAX_VM_IOCTL_SET_RAM2(), &ram_info);
    if ret != 0 {
        return errno_result();
    }
    Ok(())
}

impl Vm for HaxmVm {
    fn try_clone(&self) -> Result<Self> {
        let mut ioevents = FnvHashMap::default();
        for (addr, evt) in self.ioevents.iter() {
            ioevents.insert(*addr, evt.try_clone()?);
        }
        Ok(HaxmVm {
            vm_id: self.vm_id,
            haxm: self.haxm.try_clone()?,
            descriptor: self.descriptor.try_clone()?,
            guest_mem: self.guest_mem.clone(),
            mem_regions: self.mem_regions.clone(),
            mem_slot_gaps: self.mem_slot_gaps.clone(),
            ioevents,
        })
    }

    fn check_capability(&self, _c: VmCap) -> bool {
        false
    }

    fn get_memory(&self) -> &GuestMemory {
        &self.guest_mem
    }

    fn add_memory_region(
        &mut self,
        guest_addr: GuestAddress,
        mem: Box<dyn MappedRegion>,
        read_only: bool,
        _log_dirty_pages: bool,
    ) -> Result<MemSlot> {
        let size = mem.size() as u64;
        let end_addr = guest_addr.checked_add(size).ok_or(Error::new(EOVERFLOW))?;
        if self.guest_mem.range_overlap(guest_addr, end_addr) {
            return Err(Error::new(ENOSPC));
        }
        let mut regions = self.mem_regions.lock();
        let mut gaps = self.mem_slot_gaps.lock();
        let slot = match gaps.pop() {
            Some(gap) => gap.0,
            None => (regions.len() + self.guest_mem.num_regions() as usize) as MemSlot,
        };

        // Safe because we check that the given guest address is valid and has no overlaps. We also
        // know that the pointer and size are correct because the MemoryMapping interface ensures
        // this. We take ownership of the memory mapping so that it won't be unmapped until the slot
        // is removed.
        let res = unsafe {
            set_user_memory_region(
                &self.descriptor,
                read_only,
                guest_addr.offset(),
                size,
                MemoryRegionOp::Add(mem.as_ptr() as u64),
            )
        };

        if let Err(e) = res {
            gaps.push(Reverse(slot));
            return Err(e);
        }
        regions.insert(slot, (guest_addr, mem));
        Ok(slot)
    }

    fn msync_memory_region(&mut self, slot: MemSlot, offset: usize, size: usize) -> Result<()> {
        let mut regions = self.mem_regions.lock();
        let (_, mem) = regions.get_mut(&slot).ok_or(Error::new(ENOENT))?;

        mem.msync(offset, size).map_err(|err| match err {
            MmapError::InvalidAddress => Error::new(EFAULT),
            MmapError::NotPageAligned => Error::new(EINVAL),
            MmapError::SystemCallFailed(e) => e,
            _ => Error::new(EIO),
        })
    }

    fn remove_memory_region(&mut self, slot: MemSlot) -> Result<Box<dyn MappedRegion>> {
        let mut regions = self.mem_regions.lock();

        if let Some((guest_addr, mem)) = regions.get(&slot) {
            // Safe because the slot is checked against the list of memory slots.
            unsafe {
                set_user_memory_region(
                    &self.descriptor,
                    false,
                    guest_addr.offset(),
                    mem.size() as u64,
                    MemoryRegionOp::Remove,
                )?;
            }
            self.mem_slot_gaps.lock().push(Reverse(slot));
            Ok(regions.remove(&slot).unwrap().1)
        } else {
            Err(Error::new(ENOENT))
        }
    }

    fn create_device(&self, _kind: DeviceKind) -> Result<SafeDescriptor> {
        // Haxm does not support in-kernel devices
        Err(Error::new(libc::ENXIO))
    }

    fn get_dirty_log(&self, _slot: u32, _dirty_log: &mut [u8]) -> Result<()> {
        // Haxm does not support VmCap::DirtyLog
        Err(Error::new(libc::ENXIO))
    }

    fn register_ioevent(
        &mut self,
        evt: &Event,
        addr: IoEventAddress,
        datamatch: Datamatch,
    ) -> Result<()> {
        if datamatch != Datamatch::AnyLength {
            error!("HAXM currently only supports Datamatch::AnyLength");
            return Err(Error::new(ENOTSUP));
        }

        if self.ioevents.contains_key(&addr) {
            error!("HAXM does not support multiple ioevents for the same address");
            return Err(Error::new(EEXIST));
        }

        self.ioevents.insert(addr, evt.try_clone()?);

        Ok(())
    }

    fn unregister_ioevent(
        &mut self,
        evt: &Event,
        addr: IoEventAddress,
        datamatch: Datamatch,
    ) -> Result<()> {
        if datamatch != Datamatch::AnyLength {
            error!("HAXM only supports Datamatch::AnyLength");
            return Err(Error::new(ENOTSUP));
        }

        match self.ioevents.get(&addr) {
            Some(existing_evt) => {
                // evt should match the existing evt associated with addr
                if evt != existing_evt {
                    return Err(Error::new(ENOENT));
                }
                self.ioevents.remove(&addr);
            }

            None => {
                return Err(Error::new(ENOENT));
            }
        };
        Ok(())
    }

    /// Trigger any io events based on the memory mapped IO at `addr`.  If the hypervisor does
    /// in-kernel IO event delivery, this is a no-op.
    fn handle_io_events(&self, addr: IoEventAddress, _data: &[u8]) -> Result<()> {
        if let Some(evt) = self.ioevents.get(&addr) {
            evt.write(1)?;
        }
        Ok(())
    }

    fn get_pvclock(&self) -> Result<ClockState> {
        // Haxm does not support VmCap::PvClock
        Err(Error::new(libc::ENXIO))
    }

    fn set_pvclock(&self, _state: &ClockState) -> Result<()> {
        // Haxm does not support VmCap::PvClock
        Err(Error::new(libc::ENXIO))
    }

    fn add_fd_mapping(
        &mut self,
        slot: u32,
        offset: usize,
        size: usize,
        fd: &dyn AsRawDescriptor,
        fd_offset: u64,
        prot: Protection,
    ) -> Result<()> {
        let mut regions = self.mem_regions.lock();
        let (_, region) = regions.get_mut(&slot).ok_or(Error::new(EINVAL))?;

        match region.add_fd_mapping(offset, size, fd, fd_offset, prot) {
            Ok(()) => Ok(()),
            Err(MmapError::SystemCallFailed(e)) => Err(e),
            Err(_) => Err(Error::new(EIO)),
        }
    }

    fn remove_mapping(&mut self, slot: u32, offset: usize, size: usize) -> Result<()> {
        let mut regions = self.mem_regions.lock();
        let (_, region) = regions.get_mut(&slot).ok_or(Error::new(EINVAL))?;

        match region.remove_mapping(offset, size) {
            Ok(()) => Ok(()),
            Err(MmapError::SystemCallFailed(e)) => Err(e),
            Err(_) => Err(Error::new(EIO)),
        }
    }

    fn handle_deflate(&mut self, _guest_address: GuestAddress, _size: u64) -> Result<()> {
        // TODO(b/233773610): implement ballooning support in haxm
        warn!("Memory ballooning attempted but not supported on haxm hypervisor");
        // no-op
        Ok(())
    }

    fn handle_inflate(&mut self, _guest_address: GuestAddress, _size: u64) -> Result<()> {
        // TODO(b/233773610): implement ballooning support in haxm
        warn!("Memory ballooning attempted but not supported on haxm hypervisor");
        // no-op
        Ok(())
    }

    fn get_guest_phys_addr_bits(&self) -> u8 {
        // Assume the guest physical address size is the same as the host.
        host_phys_addr_bits()
    }
}

impl VmX86_64 for HaxmVm {
    fn get_hypervisor(&self) -> &dyn HypervisorX86_64 {
        &self.haxm
    }

    fn create_vcpu(&self, id: usize) -> Result<Box<dyn VcpuX86_64>> {
        // Safe because we know that our file is a VM fd and we verify the return result.
        let fd = unsafe { ioctl_with_ref(self, HAX_VM_IOCTL_VCPU_CREATE(), &(id as u32)) };
        if fd < 0 {
            return errno_result();
        }

        let descriptor =
            open_haxm_vcpu_device(USE_GHAXM.load(Ordering::Relaxed), self.vm_id, id as u32)?;

        let mut tunnel_info = hax_tunnel_info::default();

        // Safe because we created tunnel_info and we check the return code for errors
        let ret = unsafe {
            ioctl_with_mut_ref(&descriptor, HAX_VCPU_IOCTL_SETUP_TUNNEL(), &mut tunnel_info)
        };

        if ret != 0 {
            return errno_result();
        }

        // Safe because setup_tunnel returns the host userspace virtual address of the tunnel memory
        // mapping, along with the size of the memory mapping, and we check the return code of
        // setup_tunnel.
        let tunnel = ManuallyDrop::new(
            MemoryMapping::from_raw_ptr(tunnel_info.va as *mut c_void, tunnel_info.size as usize)
                .map_err(|_| Error::new(ENOSPC))?,
        );

        // Safe because setup_tunnel returns the host userspace virtual address of the io_buffer
        // memory mapping, which is always HAXM_IO_BUFFER_SIZE long, and we check the return
        // code of setup_tunnel.
        let io_buffer = ManuallyDrop::new(
            MemoryMapping::from_raw_ptr(tunnel_info.io_va as *mut c_void, HAXM_IO_BUFFER_SIZE)
                .map_err(|_| Error::new(ENOSPC))?,
        );

        Ok(Box::new(HaxmVcpu {
            descriptor,
            id,
            tunnel,
            io_buffer,
            vcpu_run_handle_fingerprint: Default::default(),
        }))
    }

    /// Sets the address of the three-page region in the VM's address space.
    /// This function is only necessary for 16 bit guests, which we do not support for HAXM.
    fn set_tss_addr(&self, _addr: GuestAddress) -> Result<()> {
        Ok(())
    }

    /// Sets the address of a one-page region in the VM's address space.
    /// This function is only necessary for 16 bit guests, which we do not support for HAXM.
    fn set_identity_map_addr(&self, _addr: GuestAddress) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::{EventReadResult, MemoryMappingBuilder, SharedMemory};
    use std::time::Duration;

    #[test]
    fn create_vm() {
        let haxm = Haxm::new().expect("failed to instantiate HAXM");
        let mem =
            GuestMemory::new(&[(GuestAddress(0), 0x1000)]).expect("failed to create guest memory");
        HaxmVm::new(&haxm, mem).expect("failed to create vm");
    }

    #[test]
    fn create_vcpu() {
        let haxm = Haxm::new().expect("failed to instantiate HAXM");
        let mem =
            GuestMemory::new(&[(GuestAddress(0), 0x1000)]).expect("failed to create guest memory");
        let vm = HaxmVm::new(&haxm, mem).expect("failed to create vm");
        vm.create_vcpu(0).expect("failed to create vcpu");
    }

    #[test]
    fn register_ioevent() {
        let haxm = Haxm::new().expect("failed to create haxm");
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut vm = HaxmVm::new(&haxm, gm).expect("failed to create vm");
        let evt = Event::new().expect("failed to create event");
        let otherevt = Event::new().expect("failed to create event");
        vm.register_ioevent(&evt, IoEventAddress::Pio(0xf4), Datamatch::AnyLength)
            .unwrap();
        vm.register_ioevent(&evt, IoEventAddress::Mmio(0x1000), Datamatch::AnyLength)
            .unwrap();

        vm.register_ioevent(
            &otherevt,
            IoEventAddress::Mmio(0x1000),
            Datamatch::AnyLength,
        )
        .expect_err("HAXM should not allow you to register two events for the same address");

        vm.register_ioevent(
            &otherevt,
            IoEventAddress::Mmio(0x1000),
            Datamatch::U8(None),
        )
        .expect_err(
            "HAXM should not allow you to register ioevents with Datamatches other than AnyLength",
        );

        vm.register_ioevent(
            &otherevt,
            IoEventAddress::Mmio(0x1000),
            Datamatch::U32(Some(0xf6)),
        )
        .expect_err(
            "HAXM should not allow you to register ioevents with Datamatches other than AnyLength",
        );

        vm.unregister_ioevent(&otherevt, IoEventAddress::Pio(0xf4), Datamatch::AnyLength)
            .expect_err("unregistering an unknown event should fail");
        vm.unregister_ioevent(&evt, IoEventAddress::Pio(0xf5), Datamatch::AnyLength)
            .expect_err("unregistering an unknown PIO address should fail");
        vm.unregister_ioevent(&evt, IoEventAddress::Pio(0x1000), Datamatch::AnyLength)
            .expect_err("unregistering an unknown PIO address should fail");
        vm.unregister_ioevent(&evt, IoEventAddress::Mmio(0xf4), Datamatch::AnyLength)
            .expect_err("unregistering an unknown MMIO address should fail");
        vm.unregister_ioevent(&evt, IoEventAddress::Pio(0xf4), Datamatch::AnyLength)
            .unwrap();
        vm.unregister_ioevent(&evt, IoEventAddress::Mmio(0x1000), Datamatch::AnyLength)
            .unwrap();
    }

    #[test]
    fn handle_io_events() {
        let haxm = Haxm::new().expect("failed to create haxm");
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut vm = HaxmVm::new(&haxm, gm).expect("failed to create vm");
        let evt = Event::new().expect("failed to create event");
        let evt2 = Event::new().expect("failed to create event");
        vm.register_ioevent(&evt, IoEventAddress::Pio(0x1000), Datamatch::AnyLength)
            .unwrap();
        vm.register_ioevent(&evt2, IoEventAddress::Mmio(0x1000), Datamatch::AnyLength)
            .unwrap();

        // Check a pio address
        vm.handle_io_events(IoEventAddress::Pio(0x1000), &[])
            .expect("failed to handle_io_events");
        assert_ne!(
            evt.read_timeout(Duration::from_millis(10))
                .expect("failed to read event"),
            EventReadResult::Timeout
        );
        assert_eq!(
            evt2.read_timeout(Duration::from_millis(10))
                .expect("failed to read event"),
            EventReadResult::Timeout
        );
        // Check an mmio address
        vm.handle_io_events(IoEventAddress::Mmio(0x1000), &[])
            .expect("failed to handle_io_events");
        assert_eq!(
            evt.read_timeout(Duration::from_millis(10))
                .expect("failed to read event"),
            EventReadResult::Timeout
        );
        assert_ne!(
            evt2.read_timeout(Duration::from_millis(10))
                .expect("failed to read event"),
            EventReadResult::Timeout
        );

        // Check an address that does not match any registered ioevents
        vm.handle_io_events(IoEventAddress::Pio(0x1001), &[])
            .expect("failed to handle_io_events");
        assert_eq!(
            evt.read_timeout(Duration::from_millis(10))
                .expect("failed to read event"),
            EventReadResult::Timeout
        );
        assert_eq!(
            evt2.read_timeout(Duration::from_millis(10))
                .expect("failed to read event"),
            EventReadResult::Timeout
        );
    }

    #[test]
    fn remove_memory() {
        let haxm = Haxm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut vm = HaxmVm::new(&haxm, gm).unwrap();
        let mem_size = 0x1000;
        let shm = SharedMemory::new("test", mem_size as u64).unwrap();
        let mem = MemoryMappingBuilder::new(mem_size)
            .from_shared_memory(&shm)
            .build()
            .unwrap();
        let mem_ptr = mem.as_ptr();
        let slot = vm
            .add_memory_region(GuestAddress(0x1000), Box::new(mem), false, false)
            .unwrap();
        let removed_mem = vm.remove_memory_region(slot).unwrap();
        assert_eq!(removed_mem.size(), mem_size);
        assert_eq!(removed_mem.as_ptr(), mem_ptr);
    }

    #[cfg(windows)]
    #[test]
    fn register_log_file() {
        let haxm = Haxm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = HaxmVm::new(&haxm, gm).unwrap();

        if !vm.check_raw_capability(HAX_CAP_VM_LOG) {
            return;
        }

        let dir = tempfile::TempDir::new().unwrap();
        let mut file_path = dir.path().to_owned();
        file_path.push("test");

        vm.register_log_file(file_path.to_str().unwrap())
            .expect("failed to register log file");

        let vcpu = vm.create_vcpu(0).expect("failed to create vcpu");

        // Setting cpuid will force some logs
        let cpuid = haxm.get_supported_cpuid().unwrap();
        vcpu.set_cpuid(&cpuid).expect("failed to set cpuid");

        assert!(file_path.exists());
    }
}

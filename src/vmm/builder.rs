use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use kvm_ioctls::{Cap, Kvm, VmFd};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use super::{BootConfig, MachineConfig, Result, VmmError};
use crate::arch::acpi::write_acpi_tables;
use crate::arch::layout::{
    ACPI_PM_PORT, COM_PORT_LEN, COM1_PORT, EBDA_START, HIGH_MEMORY_START, I8042_PORT,
    I8042_PORT_LEN, MMIO_GAP_END, PCI_CONFIG_PORT, PCI_CONFIG_PORT_LEN, PCI_MMIO_END,
    PCI_MMIO_START, SERIAL_IRQ, ZERO_PAGE_START, low_memory_end, ram_layout,
};
use crate::boot::bzimage::{LoadedKernel, load_bzimage};
use crate::boot::params::{E820Entry, E820Type, ZeroPage, initrd_address, write_cmdline};
use crate::devices::acpi_pm::AcpiPm;
use crate::devices::bus::Bus;
use crate::devices::i8042::I8042;
use crate::devices::pci::msix::{MsiRouter, Msix, MsixVector};
use crate::devices::pci::root::PciConfigIo;
use crate::devices::serial::Serial;
use crate::devices::virtio::VirtioDevice;
use crate::devices::virtio::block::Block;
use crate::devices::virtio::net::Net;
use crate::devices::virtio::pci::{BAR_SIZE, NotifyRegistrar, VirtioPciDevice};
use crate::exit::ExitSignal;
use crate::kvm::irq::GsiRouter;
use crate::kvm::vcpu::Vcpu;
use crate::kvm::vm::create_vm;
use crate::kvm::{IoctlContext, KvmError};
use crate::memory::GuestMemory;
use crate::sync::LockExt;
use crate::sys::tap::Tap;

const ACPI_PM_LEN: u64 = 6;
const MIN_MEMORY: u64 = 64 << 20;

const REQUIRED_CAPS: &[Cap] = &[
    Cap::Irqchip,
    Cap::Ioeventfd,
    Cap::Irqfd,
    Cap::IrqRouting,
    Cap::UserMemory,
    Cap::SetTssAddr,
    Cap::Pit2,
    Cap::PitState2,
    Cap::AdjustClock,
    Cap::VcpuEvents,
    Cap::Xsave,
    Cap::Xcrs,
    Cap::ImmediateExit,
    Cap::ExtCpuid,
    Cap::MpState,
];

pub type Shared<T> = Arc<Mutex<T>>;

pub struct Machine {
    pub kvm: Kvm,
    pub vm: Arc<VmFd>,
    pub memory: Arc<GuestMemory>,
    pub exit: Arc<ExitSignal>,
    pub mmio: Arc<Bus>,
    pub router: Shared<GsiRouter>,
    pub serial: Shared<Serial>,
    pub block: Option<Shared<VirtioPciDevice<Block>>>,
    pub net: Option<Shared<VirtioPciDevice<Net>>>,
    pub vcpus: Vec<Vcpu>,
}

fn open_kvm() -> Result<Kvm> {
    let kvm = Kvm::new().map_err(VmmError::KvmUnavailable)?;
    for cap in REQUIRED_CAPS {
        if !kvm.check_extension(*cap) {
            return Err(VmmError::MissingCapability(*cap));
        }
    }
    Ok(kvm)
}

fn validate(config: &MachineConfig, kvm: &Kvm) -> Result<()> {
    if config.cpus == 0 || usize::from(config.cpus) > kvm.get_max_vcpus() {
        return Err(VmmError::InvalidConfig(format!(
            "vCPU count must be between 1 and {}",
            kvm.get_max_vcpus()
        )));
    }
    if config.mem_size < MIN_MEMORY || !config.mem_size.is_multiple_of(4096) {
        return Err(VmmError::InvalidConfig(
            "memory must be a multiple of 4 KiB and at least 64 MiB".into(),
        ));
    }
    Ok(())
}

struct BarAllocator {
    next: u64,
}

impl BarAllocator {
    fn allocate(&mut self, size: u64) -> Result<u32> {
        let address = self.next.next_multiple_of(size);
        if address + size > PCI_MMIO_END {
            return Err(VmmError::InvalidConfig("PCI MMIO window exhausted".into()));
        }
        self.next = address + size;
        Ok(address as u32)
    }
}

struct VirtioWiring<'a> {
    vm: &'a Arc<VmFd>,
    memory: &'a Arc<GuestMemory>,
    router: &'a Shared<GsiRouter>,
    mmio: &'a Arc<Bus>,
    pci: &'a mut PciConfigIo,
    bars: &'a mut BarAllocator,
}

impl VirtioWiring<'_> {
    fn attach<D: VirtioDevice + 'static>(
        &mut self,
        device: D,
    ) -> Result<Shared<VirtioPciDevice<D>>> {
        let queue_count = device.queue_max_sizes().len();
        let mut vectors = Vec::with_capacity(queue_count + 1);
        for _ in 0..=queue_count {
            let gsi = self.router.lock_or_recover().allocate()?;
            let event = EventFd::new(EFD_NONBLOCK)?;
            self.vm.register_irqfd(&event, gsi).ctx("KVM_IRQFD")?;
            vectors.push(MsixVector {
                gsi,
                interrupt: Box::new(event),
            });
        }
        let router: Arc<dyn MsiRouter> = self.router.clone();
        let msix = Msix::new(vectors, router);
        let notify_events = (0..queue_count)
            .map(|_| EventFd::new(EFD_NONBLOCK))
            .collect::<io::Result<Vec<_>>>()?;
        let bar = self.bars.allocate(u64::from(BAR_SIZE))?;
        let registrar: Arc<dyn NotifyRegistrar> = self.vm.clone();
        let device = Arc::new(Mutex::new(VirtioPciDevice::new(
            device,
            self.memory.clone(),
            msix,
            notify_events,
            registrar,
            bar,
        )?));
        self.mmio
            .insert(device.clone(), u64::from(bar), u64::from(BAR_SIZE))?;
        self.pci
            .add_device(device.clone())
            .ok_or_else(|| VmmError::InvalidConfig("no free PCI slot".into()))?;
        Ok(device)
    }
}

pub fn build_machine(config: &MachineConfig) -> Result<Machine> {
    let kvm = open_kvm()?;
    validate(config, &kvm)?;
    let memory = Arc::new(GuestMemory::new(&ram_layout(config.mem_size))?);
    let vm = create_vm(&kvm, &memory)?;
    let exit = Arc::new(ExitSignal::new()?);
    let pio = Arc::new(Bus::new());
    let mmio = Arc::new(Bus::new());
    let router = Arc::new(Mutex::new(GsiRouter::new(vm.clone())));

    let serial_irq = EventFd::new(EFD_NONBLOCK)?;
    vm.register_irqfd(&serial_irq, SERIAL_IRQ)
        .ctx("KVM_IRQFD")?;
    let serial = Arc::new(Mutex::new(Serial::new(
        Box::new(serial_irq),
        Box::new(io::stdout()),
    )));
    pio.insert(serial.clone(), COM1_PORT, COM_PORT_LEN)?;
    pio.insert(
        Arc::new(Mutex::new(I8042::new(exit.clone()))),
        I8042_PORT,
        I8042_PORT_LEN,
    )?;
    pio.insert(
        Arc::new(Mutex::new(AcpiPm::new(exit.clone()))),
        u64::from(ACPI_PM_PORT),
        ACPI_PM_LEN,
    )?;

    let mut pci = PciConfigIo::new(mmio.clone());
    let mut bars = BarAllocator {
        next: PCI_MMIO_START,
    };
    let mut wiring = VirtioWiring {
        vm: &vm,
        memory: &memory,
        router: &router,
        mmio: &mmio,
        pci: &mut pci,
        bars: &mut bars,
    };
    let block = match &config.disk {
        Some(disk) => {
            let device =
                Block::open(&disk.path, disk.read_only).map_err(|source| VmmError::OpenDevice {
                    what: disk.path.display().to_string(),
                    source,
                })?;
            Some(wiring.attach(device)?)
        }
        None => None,
    };
    let net = match &config.net {
        Some(net) => {
            let tap = Tap::open(&net.tap).map_err(|source| VmmError::OpenDevice {
                what: format!("tap device {}", net.tap),
                source,
            })?;
            Some(wiring.attach(Net::new(Box::new(tap), net.mac))?)
        }
        None => None,
    };
    pio.insert(
        Arc::new(Mutex::new(pci)),
        PCI_CONFIG_PORT,
        PCI_CONFIG_PORT_LEN,
    )?;
    router.lock_or_recover().commit()?;

    let vcpus = (0..config.cpus)
        .map(|index| Vcpu::new(&kvm, &vm, index, pio.clone(), mmio.clone(), exit.clone()))
        .collect::<std::result::Result<Vec<_>, KvmError>>()?;

    Ok(Machine {
        kvm,
        vm,
        memory,
        exit,
        mmio,
        router,
        serial,
        block,
        net,
        vcpus,
    })
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|source| VmmError::OpenDevice {
        what: path.display().to_string(),
        source,
    })
}

pub fn e820_map(mem_size: u64) -> Vec<E820Entry> {
    let low_end = low_memory_end(mem_size);
    let mut map = vec![
        E820Entry {
            addr: 0,
            size: EBDA_START.raw(),
            kind: E820Type::Ram,
        },
        E820Entry {
            addr: EBDA_START.raw(),
            size: HIGH_MEMORY_START.raw() - EBDA_START.raw(),
            kind: E820Type::Reserved,
        },
        E820Entry {
            addr: HIGH_MEMORY_START.raw(),
            size: low_end - HIGH_MEMORY_START.raw(),
            kind: E820Type::Ram,
        },
    ];
    if mem_size > low_end {
        map.push(E820Entry {
            addr: MMIO_GAP_END,
            size: mem_size - low_end,
            kind: E820Type::Ram,
        });
    }
    map
}

pub fn load_guest(
    boot: &BootConfig,
    config: &MachineConfig,
    memory: &GuestMemory,
) -> Result<LoadedKernel> {
    let low_end = low_memory_end(config.mem_size);
    let image = read_file(&boot.kernel)?;
    let kernel = load_bzimage(memory, &image, low_end)?;
    let mut zero_page = ZeroPage::new(&kernel.header);
    zero_page.set_cmdline(write_cmdline(memory, &kernel.header, &boot.cmdline)?);
    if let Some(path) = &boot.initrd {
        let initrd = read_file(path)?;
        let addr = initrd_address(&kernel, initrd.len(), low_end)?;
        memory.write(addr, &initrd)?;
        zero_page.set_initrd(addr, initrd.len() as u64);
    }
    for entry in e820_map(config.mem_size) {
        zero_page.add_e820(entry);
    }
    zero_page.write_to(memory, ZERO_PAGE_START)?;
    write_acpi_tables(memory, config.cpus)?;
    Ok(kernel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e820_without_gap() {
        let map = e820_map(512 << 20);
        assert_eq!(map.len(), 3);
        assert_eq!(map[2].addr + map[2].size, 512 << 20);
        assert_eq!(map[1].kind, E820Type::Reserved);
    }

    #[test]
    fn e820_with_gap() {
        let map = e820_map(5 << 30);
        assert_eq!(map.len(), 4);
        assert_eq!(map[2].addr + map[2].size, 3 << 30);
        assert_eq!((map[3].addr, map[3].size), (4 << 30, 2 << 30));
    }

    #[test]
    fn bars_are_aligned_and_bounded() {
        let mut bars = BarAllocator {
            next: PCI_MMIO_START + 0x100,
        };
        assert_eq!(bars.allocate(0x8000).unwrap(), 0xc000_8000);
        assert_eq!(bars.allocate(0x8000).unwrap(), 0xc001_0000);
        let mut full = BarAllocator {
            next: PCI_MMIO_END - 0x4000,
        };
        assert!(full.allocate(0x8000).is_err());
    }
}

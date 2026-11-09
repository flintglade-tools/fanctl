// SPDX-License-Identifier: GPL-2.0

//! Fan control for the Nuvoton NCT6687D embedded controller on the MSI PRO Z690-A WIFI (MS-7D25).
//!
//! `/dev/nct6687` answers two ioctls: a sensor snapshot and a manual duty request for one fan.
//! Manual control belongs to the open file that requested it; when that file is closed for any
//! reason, including the process crashing or being killed, every fan it changed returns to the
//! exact firmware (BIOS curve) state saved before its first change. The CPU fan and pump headers never drop below 30 %, and requests for them are raised
//! to 100 % while the CPU reads 85 °C or more.
//!
//! The EC protocol (page/index/data window and the fan-configuration handshake) follows the
//! upstream `nct6683` driver and the out-of-tree `nct6687d` driver.

use core::arch::asm;
use kernel::{
    bindings,
    fs::{file, File},
    ioctl::{_IO, _IOR},
    error::to_result,
    miscdevice::{MiscDevice, MiscDeviceOptions, MiscDeviceRegistration},
    prelude::*,
    time::{delay::fsleep, Delta},
    types::Opaque,
    uaccess::UserSlice,
};

module! {
    type: Nct6687Module,
    name: "nct6687_rs",
    authors: ["Flintglade"],
    description: "NCT6687D fan control for MSI PRO Z690-A WIFI",
    license: "GPL",
}

const BOARD: &[u8] = b"PRO Z690-A WIFI (MS-7D25)";
const NAME: &CStr = c"nct6687_rs";

const MAGIC: u32 = b'N' as u32;
const SNAPSHOT_LEN: usize = 56;
/// Fills a `SNAPSHOT_LEN`-byte buffer; layout in [`Ec::snapshot`].
const IOC_SNAPSHOT: u32 = _IOR::<[u8; SNAPSHOT_LEN]>(MAGIC, 0xa0);
/// `arg = fan << 8 | duty`; returns the duty actually applied.
const IOC_SET_DUTY: u32 = _IO(MAGIC, 0xa1);

const FANS: usize = 8;
const TEMPS: usize = 7;
/// Minimum duty for CPU_FAN1 and PUMP_FAN1 (77/255 = 30 %).
const FLOOR: [u8; FANS] = [77, 77, 0, 0, 0, 0, 0, 0];
const HOT_C: i8 = 85;

const SIO_PORTS: [u16; 2] = [0x2e, 0x4e];
const SIO_LD_HWM: u8 = 0x0b;
const SIO_ID_NCT6687: u16 = 0xd590;

const EC_PAGE: u16 = 4;
const EC_INDEX: u16 = 5;
const EC_DATA: u16 = 6;

const fn reg_temp(i: usize) -> u16 { 0x100 + 2 * i as u16 }
const fn reg_rpm(i: usize) -> u16 { 0x140 + 2 * i as u16 }
const fn reg_duty(i: usize) -> u16 { 0x160 + i as u16 }
const fn reg_duty_write(i: usize) -> u16 { 0xa28 + i as u16 }
const REG_CTRL_MODE: u16 = 0xa00;
const REG_CFG_COMMAND: u16 = 0xa01;
const REG_ENGINE_STATUS: u16 = 0xcf8;

const CFG_REQUEST: u8 = 0x80;
const CFG_DONE: u8 = 0x40;
const STS_PHASE: u8 = 1 << 3;
const STS_INVALID: u8 = 1 << 4;
const STS_CHECK_DONE: u8 = 1 << 5;
const STS_LOCK: u8 = 1 << 6;

extern "C" {
    fn dmi_get_system_info(field: c_int) -> *const c_char;
}

fn outb(port: u16, value: u8) {
    // SAFETY: Callers only address the Super-I/O pair they reserved or this driver's EC window.
    // Port output does not touch Rust-visible memory.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) }
}

fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: As in `outb`.
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags)) }
    value
}

/// `outb_p`/`inb_p` pacing under this kernel's CONFIG_IO_DELAY_0X80.
fn io_delay() {
    outb(0x80, 0);
}

fn board_matches() -> bool {
    // SAFETY: `dmi_get_system_info` returns null or a NUL-terminated string that lives as long
    // as the kernel.
    let name = unsafe { dmi_get_system_info(bindings::dmi_field_DMI_BOARD_NAME as c_int) };
    // SAFETY: Non-null results are valid NUL-terminated strings (see above).
    !name.is_null() && unsafe { CStr::from_char_ptr(name) }.to_bytes() == BOARD
}

/// Finds the EC window of an enabled NCT6687D on the Super-I/O pair at `sio`.
fn probe(sio: u16) -> Option<u16> {
    // SAFETY: Reserves the shared Super-I/O pair the way `request_muxed_region` does.
    let region = unsafe {
        bindings::__request_region(
            &raw mut bindings::ioport_resource,
            sio.into(),
            2,
            NAME.as_char_ptr(),
            bindings::IORESOURCE_MUXED as c_int,
        )
    };
    if region.is_null() {
        return None;
    }
    let read = |reg: u8| {
        outb(sio, reg);
        inb(sio + 1)
    };
    outb(sio, 0x87);
    outb(sio, 0x87);
    let id = u16::from(read(0x20)) << 8 | u16::from(read(0x21));
    let mut base = None;
    if id & 0xfff0 == SIO_ID_NCT6687 {
        outb(sio, 0x07);
        outb(sio + 1, SIO_LD_HWM);
        let mut address = u16::from(read(0x60)) << 8 | u16::from(read(0x61));
        if address & 0x07 == 0x05 {
            address &= 0xfff8;
        }
        if read(0x30) & 1 == 1 && address >= 0x100 && address & 0xf007 == 0 {
            base = Some(address);
        }
    }
    outb(sio, 0xaa);
    outb(sio, 0x02);
    outb(sio + 1, 0x02);
    // SAFETY: Releases the reservation made above.
    unsafe { bindings::__release_region(&raw mut bindings::ioport_resource, sio.into(), 2) };
    base
}

#[derive(Clone, Copy)]
struct Saved {
    manual: bool,
    target: u8,
}

struct Ec {
    /// EC window base; the window itself is `base + 4 ..= base + 7`.
    base: u16,
    /// Open-file id holding manual control, or 0.
    owner: u64,
    next_id: u64,
    saved: [Option<Saved>; FANS],
    applied: [Option<u8>; FANS],
}

kernel::sync::global_lock! {
    // SAFETY: Initialized in module init before the device is registered.
    unsafe(uninit) static EC: Mutex<Ec> = Ec {
        base: 0, owner: 0, next_id: 1, saved: [None; FANS], applied: [None; FANS],
    };
}

impl Ec {
    fn read(&self, reg: u16) -> u8 {
        let port = self.base + EC_PAGE;
        outb(port, 0xff);
        io_delay();
        outb(port, (reg >> 8) as u8);
        io_delay();
        outb(self.base + EC_INDEX, reg as u8);
        io_delay();
        let value = inb(self.base + EC_DATA);
        io_delay();
        value
    }

    fn write(&self, reg: u16, value: u8) {
        let port = self.base + EC_PAGE;
        outb(port, 0xff);
        io_delay();
        outb(port, (reg >> 8) as u8);
        io_delay();
        outb(self.base + EC_INDEX, reg as u8);
        io_delay();
        outb(self.base + EC_DATA, value);
        io_delay();
    }

    /// Polls for up to one second.
    fn wait(&self, mut ready: impl FnMut(&Self) -> bool) -> Result {
        for _ in 0..1000 {
            if ready(self) {
                return Ok(());
            }
            fsleep(Delta::from_millis(1));
        }
        Err(EIO)
    }

    /// Asks the EC fan engine to unlock its configuration registers.
    fn begin_config(&self) -> Result {
        let status = self.read(REG_ENGINE_STATUS);
        if status & STS_LOCK == 0 && status & STS_PHASE != 0 {
            return Ok(());
        }
        self.wait(|ec| {
            ec.read(REG_ENGINE_STATUS) & STS_PHASE == 0 && ec.read(REG_CFG_COMMAND) & CFG_REQUEST == 0
        })
        .inspect_err(|_| pr_err!("EC stuck in a configuration phase\n"))?;
        self.write(REG_CFG_COMMAND, CFG_REQUEST);
        self.wait(|ec| {
            let status = ec.read(REG_ENGINE_STATUS);
            status & STS_LOCK == 0 && status & STS_PHASE != 0
        })
        .inspect_err(|_| pr_err!("EC did not unlock fan configuration\n"))
    }

    /// Commits the configuration and waits for the EC to accept and relock it.
    fn end_config(&self) -> Result {
        let status = self.read(REG_ENGINE_STATUS);
        if status & STS_LOCK != 0 || status & STS_PHASE == 0 {
            pr_err!("fan configuration is not open (status {:#04x})\n", status);
            return Err(EIO);
        }
        self.write(REG_CFG_COMMAND, CFG_DONE);
        self.wait(|ec| ec.read(REG_ENGINE_STATUS) & STS_CHECK_DONE != 0)
            .inspect_err(|_| pr_err!("EC did not check the new fan configuration\n"))?;
        let status = self.read(REG_ENGINE_STATUS);
        if status & STS_INVALID != 0 || status & STS_LOCK == 0 {
            pr_err!("EC rejected the fan configuration (status {:#04x})\n", status);
            return Err(EIO);
        }
        Ok(())
    }

    fn cpu_temp(&self) -> i8 {
        self.read(reg_temp(0)) as i8
    }

    /// Layout (little-endian): 7 × i32 temperatures in m°C, 8 × u16 RPM, 8 × u8 current duty,
    /// u8 manual-mode mask from the EC, u8 mask of fans held by this driver, 2 bytes padding.
    fn snapshot(&self) -> [u8; SNAPSHOT_LEN] {
        let mut out = [0u8; SNAPSHOT_LEN];
        for i in 0..TEMPS {
            let whole = i32::from(self.read(reg_temp(i)) as i8);
            let half = i32::from(self.read(reg_temp(i) + 1) >> 7);
            out[i * 4..i * 4 + 4].copy_from_slice(&(whole * 1000 + half * 500).to_le_bytes());
        }
        for i in 0..FANS {
            let rpm = u16::from(self.read(reg_rpm(i))) << 8 | u16::from(self.read(reg_rpm(i) + 1));
            out[28 + i * 2..30 + i * 2].copy_from_slice(&rpm.to_le_bytes());
            out[44 + i] = self.read(reg_duty(i));
        }
        out[52] = self.read(REG_CTRL_MODE);
        out[53] = (0..FANS).filter(|&i| self.saved[i].is_some()).fold(0, |mask, i| mask | 1 << i);
        out
    }

    fn save(&mut self, fan: usize) -> Result {
        if self.saved[fan].is_some() {
            return Ok(());
        }
        let manual = self.read(REG_CTRL_MODE) & (1 << fan) != 0;
        self.begin_config()?;
        let target = self.read(reg_duty_write(fan));
        self.end_config()?;
        self.saved[fan] = Some(Saved { manual, target });
        Ok(())
    }

    fn set_duty(&mut self, fan: usize, requested: u8) -> Result<u8> {
        let mut duty = requested.max(FLOOR[fan]);
        if FLOOR[fan] > 0 && self.cpu_temp() >= HOT_C {
            duty = u8::MAX;
        }
        if self.applied[fan] == Some(duty) {
            return Ok(duty);
        }
        self.save(fan)?;
        self.applied[fan] = None;
        self.begin_config()?;
        let bit = 1 << fan;
        self.write(REG_CTRL_MODE, self.read(REG_CTRL_MODE) | bit);
        self.write(reg_duty_write(fan), duty);
        let written = self.read(reg_duty_write(fan)) == duty;
        self.end_config()?;
        if !written || self.read(REG_CTRL_MODE) & bit == 0 {
            pr_err!("fan {} did not accept manual duty {}\n", fan, duty);
            return Err(EIO);
        }
        self.applied[fan] = Some(duty);
        Ok(duty)
    }

    fn restore(&mut self, fan: usize) -> Result {
        let Some(saved) = self.saved[fan] else { return Ok(()) };
        self.applied[fan] = None;
        let target = self.begin_config().and_then(|()| {
            self.write(reg_duty_write(fan), saved.target);
            let written = self.read(reg_duty_write(fan)) == saved.target;
            self.end_config()?;
            if written { Ok(()) } else { Err(EIO) }
        });
        // The mode bit goes back even if the handshake failed: once it is clear, the BIOS curve
        // drives the fan whatever the target register holds.
        let bit = 1 << fan;
        let mode = self.read(REG_CTRL_MODE) & !bit | if saved.manual { bit } else { 0 };
        self.write(REG_CTRL_MODE, mode);
        if self.read(REG_CTRL_MODE) & bit != mode & bit || target.is_err() {
            pr_err!("fan {} did not fully return to its firmware state\n", fan);
            return Err(EIO);
        }
        self.saved[fan] = None;
        Ok(())
    }

    fn restore_all(&mut self) {
        for fan in 0..FANS {
            if self.restore(fan).is_err() {
                // The saved state stays, so the next release or the module unload retries it.
                pr_err!("could not restore fan {}; the BIOS curve resumes after reboot\n", fan);
            }
        }
    }

    /// Gives manual control to `id` unless another open file already holds it.
    fn claim(&mut self, id: u64) -> Result {
        match self.owner {
            0 => {
                self.owner = id;
                Ok(())
            }
            owner if owner == id => Ok(()),
            _ => Err(EBUSY),
        }
    }
}

struct Handle {
    id: u64,
}

#[vtable]
impl MiscDevice for Handle {
    type Ptr = KBox<Self>;

    fn open(_file: &File, _misc: &MiscDeviceRegistration<Self>) -> Result<KBox<Self>> {
        let mut ec = EC.lock();
        let id = ec.next_id;
        ec.next_id += 1;
        Ok(KBox::new(Handle { id }, GFP_KERNEL)?)
    }

    fn release(handle: KBox<Self>, _file: &File) {
        let mut ec = EC.lock();
        if ec.owner == handle.id {
            ec.restore_all();
            ec.owner = 0;
        }
    }

    fn ioctl(handle: &Handle, file: &File, cmd: u32, arg: usize) -> Result<isize> {
        if cmd == IOC_SNAPSHOT {
            let snapshot = EC.lock().snapshot();
            UserSlice::new(UserPtr::from_addr(arg), SNAPSHOT_LEN).writer().write_slice(&snapshot)?;
            return Ok(0);
        }
        if cmd != IOC_SET_DUTY {
            return Err(ENOTTY);
        }
        if file.flags() & file::flags::O_ACCMODE == file::flags::O_RDONLY {
            return Err(EBADF);
        }
        let fan = arg >> 8;
        if fan >= FANS {
            return Err(EINVAL);
        }
        let mut ec = EC.lock();
        ec.claim(handle.id)?;
        Ok(ec.set_duty(fan, arg as u8)?.into())
    }
}

/// The reserved I/O range `base ..= base + 7` (the EC window is `base + 4 ..= base + 6`).
struct Window(u16);

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: Releases the range reserved in `reserve`.
        unsafe { bindings::__release_region(&raw mut bindings::ioport_resource, self.0.into(), 8) };
    }
}

/// File operations of `/dev/nct6687`: the misc-device trampolines for `Handle`, plus `owner`.
static mut FOPS: bindings::file_operations = unsafe { core::mem::zeroed() };

/// `/dev/nct6687`, registered here rather than through `MiscDeviceRegistration` because that
/// leaves `file_operations.owner` unset. With `owner` set, every open file pins this module, so it
/// cannot be unloaded while a process holds fan control.
#[pin_data(PinnedDrop)]
struct Device {
    #[pin]
    inner: Opaque<bindings::miscdevice>,
}

// SAFETY: As for `MiscDeviceRegistration`: `misc_deregister` may run on any thread, and the
// registered `struct miscdevice` is only touched by the misc core under its own lock.
unsafe impl Send for Device {}
// SAFETY: `Device` has no `&self` methods.
unsafe impl Sync for Device {}

impl Device {
    fn register() -> impl PinInit<Self, Error> {
        try_pin_init!(Self {
            inner <- Opaque::try_ffi_init(|slot: *mut bindings::miscdevice| {
                let mut raw = MiscDeviceOptions { name: c"nct6687" }.into_raw::<Handle>();
                let fops = &raw mut FOPS;
                // SAFETY: Module init runs once and before the device exists, so nothing else
                // reads `FOPS` yet. The trampolines only need `private_data` to point at this
                // `struct miscdevice`, the layout `MiscDeviceRegistration` wraps transparently.
                unsafe {
                    fops.write(raw.fops.read());
                    (*fops).owner = THIS_MODULE.as_ptr();
                    raw.fops = fops;
                    slot.write(raw);
                    to_result(bindings::misc_register(slot))
                }
            }),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for Device {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: `register` succeeded, or this value would not exist.
        unsafe { bindings::misc_deregister(self.inner.get()) };
    }
}

// Fields drop in order: the device disappears before the I/O range is released.
#[pin_data(PinnedDrop)]
struct Nct6687Module {
    #[pin]
    _device: Device,
    _window: Window,
}

impl kernel::InPlaceModule for Nct6687Module {
    fn init(_module: &'static ThisModule) -> impl PinInit<Self, Error> {
        try_pin_init!(Self {
            _window: reserve()?,
            _device <- Device::register(),
        })
    }
}

/// Locates and reserves the EC window, then initializes the shared state.
fn reserve() -> Result<Window> {
    if !board_matches() {
        pr_err!("refusing to load: this driver's fan map is only verified on the MSI PRO Z690-A WIFI\n");
        return Err(ENODEV);
    }
    let base = SIO_PORTS.iter().find_map(|&sio| probe(sio)).ok_or(ENODEV)?;
    // SAFETY: Reserves the range while the module is loaded; `Window` releases it. It covers both
    // ranges other drivers claim for this chip (nct6683: base + 4, nct6687d: base), so neither
    // can load alongside this one.
    let region = unsafe {
        bindings::__request_region(&raw mut bindings::ioport_resource, base.into(), 8, NAME.as_char_ptr(), 0)
    };
    if region.is_null() {
        pr_err!("I/O range {:#x} is busy; unload nct6683 first\n", base);
        return Err(EBUSY);
    }
    // SAFETY: Called once, before the device exists and therefore before any other use.
    unsafe { EC.init() };
    EC.lock().base = base;
    pr_info!("NCT6687D EC at {:#x}\n", base);
    Ok(Window(base))
}

#[pinned_drop]
impl PinnedDrop for Nct6687Module {
    fn drop(self: Pin<&mut Self>) {
        EC.lock().restore_all();
    }
}

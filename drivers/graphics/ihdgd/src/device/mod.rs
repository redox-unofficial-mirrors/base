use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::mem::DropGuard;
use std::sync::Arc;

use common::io::{Io, MmioPtr};
use common::timeout::Timeout;
use driver_graphics::kms::connector::{modeinfo_for_size, KmsConnectorStatus};
use driver_graphics::kms::objects::{KmsObjectId, KmsObjects};
use driver_graphics::GraphicsScheme;
use drm_sys::drm_mode_modeinfo;
use ihdgd_macros::define_regs;
use pcid_interface::{PciFunction, PciFunctionHandle};
use piaf::{capabilities_from_edid, EdidSource, ExtensionLibrary};
use range_alloc::RangeAllocator;
use syscall::error::{Error, Result, EIO, ENODEV, ERANGE};

mod aux;
mod bios;
use self::bios::*;
mod buffer;
use self::buffer::*;
mod ddi;
use self::ddi::*;
mod dpll;
use self::dpll::*;
mod gmbus;
pub use self::gmbus::*;
mod gpio;
pub use self::gpio::*;
mod ggtt;
use ggtt::*;
mod hal;
pub use self::hal::*;
mod pipe;
use self::pipe::*;
mod power;
use self::power::*;
mod scheme;
use self::scheme::*;
mod transcoder;
use self::transcoder::*;

pub struct ChangeDetect {
    name: &'static str,
    reg: MmioPtr<u32>,
    value: u32,
}

impl ChangeDetect {
    fn new(name: &'static str, reg: MmioPtr<u32>) -> Self {
        let value = reg.read();
        Self { name, reg, value }
    }

    fn log(&self) {
        log::info!("{} {:08X}", self.name, self.value);
    }

    fn check(&mut self) {
        let value = self.reg.read();
        if value != self.value {
            self.value = value;
            self.log();
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum DeviceKind {
    KabyLake,
    TigerLake,
    AlderLakeP,
    Alchemist,
    MeteorLakeP,
}

pub enum Event {
    DdiHotplug(&'static str),
}

define_regs! {
    pub struct InterruptRegs {
        // Interrupt status register, has live status of interrupts
        reg isr: u32,
        // Interrupt mask register, masks isr for iir, 0 is unmasked
        reg imr: u32,
        // Interrupt identity register, write 1 to clear
        reg iir: u32,
        // Interrupt enable register, 1 allows interrupt to propogate
        reg ier: u32,
    }
}

impl InterruptRegs {
    pub unsafe fn new(gttmm: &MmioRegion, base: usize) -> Result<Self> {
        Ok(InterruptRegs {
            isr: unsafe { gttmm.mmio(base + 0x0)? },
            imr: unsafe { gttmm.mmio(base + 0x4)? },
            iir: unsafe { gttmm.mmio(base + 0x8)? },
            ier: unsafe { gttmm.mmio(base + 0xC)? },
        })
    }

    /// Enable interrupts with mask
    pub fn enable(&mut self, mask: u32) {
        // Set interrupt enable mask
        self.ier.write(mask);
        // Clear identity register
        self.iir.write(self.iir.read());
        // Unmask all interrupts
        self.imr.write(0);
    }

    /// Read pending interrupts
    pub fn pending(&mut self) -> u32 {
        let mask = self.iir.read();
        self.iir.write(mask);
        mask
    }
}

define_regs! {
    pub struct Interrupter {
        let change_detects: Vec<ChangeDetect>,
        reg display_int_ctl: u32 {
            flag enable,
            flag sde,
        },
        reg gfx_mstr_intr?: u32 {
            flag display,
            flag enable,
        },
        let sde_interrupt: InterruptRegs,
    }
}

#[derive(Debug)]
pub struct MmioRegion {
    virt: usize,
    size: usize,
}

impl MmioRegion {
    unsafe fn new(phys: usize, size: usize, memory_type: common::MemoryType) -> Result<Self> {
        let virt = unsafe { common::physmap(phys, size, common::Prot::RW, memory_type)? as usize };
        Ok(Self { virt, size })
    }

    unsafe fn new_pci_bar(
        pcid_handle: &mut PciFunctionHandle,
        bir: u8,
        memory_type: common::MemoryType,
    ) -> Result<Self> {
        let mapped_bar = unsafe { pcid_handle.map_bar(bir, memory_type) };
        Ok(Self {
            virt: mapped_bar.ptr.expose_provenance().get(),
            size: mapped_bar.bar_size,
        })
    }

    unsafe fn mmio(&self, offset: usize) -> Result<MmioPtr<u32>> {
        // Any errors here will return ERANGE
        let err = Error::new(ERANGE);
        if offset.checked_add(size_of::<u32>()).ok_or(err)? > self.size {
            return Err(err);
        }
        let addr = self.virt.checked_add(offset).ok_or(err)?;
        Ok(unsafe { MmioPtr::new(addr as *mut u32) })
    }
}

impl Drop for MmioRegion {
    fn drop(&mut self) {
        unsafe {
            let _ = libredox::call::munmap(self.virt as *mut (), self.size);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum VideoInput {
    Hdmi,
    Dp,
}

pub struct Device {
    unique: String,
    kind: DeviceKind,
    alloc_buffers: RangeAllocator<u32>,
    bios: Option<Bios>,
    ddis: Vec<Ddi>,
    dpclka_cfgcr0: Option<MmioPtr<u32>>,
    dplls: Vec<Dpll>,
    events: VecDeque<Event>,
    int: Interrupter,
    gttmm: Arc<MmioRegion>,
    ggtt: GlobalGtt,
    gm: MmioRegion,
    gmbus: Gmbus,
    pipes: Vec<Pipe>,
    power_wells: PowerWells,
    ref_freq: u64,
    transcoders: Vec<Transcoder>,
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("kind", &self.kind)
            .field("alloc_buffers", &self.alloc_buffers)
            .field("gttmm", &self.gttmm)
            .field("gm", &self.gm)
            .field("ref_freq", &self.ref_freq)
            .finish_non_exhaustive()
    }
}

impl Device {
    pub fn new(pcid_handle: &mut PciFunctionHandle, func: &PciFunction) -> Result<Self> {
        let kind = match (func.full_device_id.vendor_id, func.full_device_id.device_id) {
            // Kaby Lake
            (0x8086, 0x5912) |
            (0x8086, 0x5916) |
            (0x8086, 0x591B) |
            (0x8086, 0x591E) |
            (0x8086, 0x5926) |
            // Comet Lake, seems to be compatible with Kaby Lake
            (0x8086, 0x9B21) |
            (0x8086, 0x9B41) |
            (0x8086, 0x9BA4) |
            (0x8086, 0x9BAA) |
            (0x8086, 0x9BAC) |
            (0x8086, 0x9BC4) |
            (0x8086, 0x9BC5) |
            (0x8086, 0x9BC6) |
            (0x8086, 0x9BC8) |
            (0x8086, 0x9BCA) |
            (0x8086, 0x9BCC) |
            (0x8086, 0x9BE6) |
            (0x8086, 0x9BF6) => {
                DeviceKind::KabyLake
            }
            // Tiger Lake
            (0x8086, 0x9A40) |
            (0x8086, 0x9A49) |
            (0x8086, 0x9A60) |
            (0x8086, 0x9A68) |
            (0x8086, 0x9A70) |
            (0x8086, 0x9A78) => {
                DeviceKind::TigerLake
            }
            // Alder Lake-P
            //TODO: add more IDs
            (0x8086, 0x46a6) | // Alder Lake-P GT2
            (0x8086, 0x46a8)   // Alder Lake-UP3 GT2
            => {
                DeviceKind::AlderLakeP
            }
            // Alchemist
            (0x8086, 0x5690) | // A770M
            (0x8086, 0x5691) | // A730M
            (0x8086, 0x5692) | // A550M
            (0x8086, 0x5693) | // A370M
            (0x8086, 0x5694) | // A350M
            (0x8086, 0x5696) | // A570M
            (0x8086, 0x5697) | // A530M
            (0x8086, 0x56A0) | // A770
            (0x8086, 0x56A1) | // A750
            (0x8086, 0x56A5) | // A380
            (0x8086, 0x56A6) | // A310
            (0x8086, 0x56B0) | // Pro A30M
            (0x8086, 0x56B1) | // Pro A40/A50
            (0x8086, 0x56B2) | // Pro A60M
            (0x8086, 0x56B3) | // Pro A60
            (0x8086, 0x56C0) | // GPU Flex 170
            (0x8086, 0x56C1)   // GPU Flex 140
            => {
                DeviceKind::Alchemist
            }
            // Meteor Lake-P
            //TODO: add more IDs
            (0x8086, 0x7d45) | // Meteor Lake-P
            (0x8086, 0x7dd5)   // Meteor Lake-P
            => {
                DeviceKind::MeteorLakeP
            }
            (vendor_id, device_id) => {
                log::error!("unsupported ID {:04X}:{:04X}", vendor_id, device_id);
                return Err(Error::new(ENODEV));
            }
        };

        log::info!(
            "{:04X}:{:04X}: {:?}",
            func.full_device_id.vendor_id,
            func.full_device_id.device_id,
            kind
        );

        let gttmm = {
            Arc::new(unsafe {
                MmioRegion::new_pci_bar(pcid_handle, 0, common::MemoryType::Uncacheable)
            }?)
        };
        log::info!("GTTMM {:X?}", gttmm);
        let gm =
            unsafe { MmioRegion::new_pci_bar(pcid_handle, 2, common::MemoryType::WriteCombining) }?;
        log::info!("GM {:X?}", gm);
        /* IOBAR not used, not present on all generations
        let iobar = func.bars[4].expect_port();
        log::debug!("IOBAR {:X?}", iobar);
        */

        // IGD OpRegion/Software SCI/_DSM for Skylake Processors
        let bios_base = unsafe { pcid_handle.read_config(0xFC) };
        let bios = if bios_base != 0 {
            log::info!("BIOS {:X?}", bios_base);
            // This is the default BIOS size
            let bios_size = 8 * 1024;
            match unsafe {
                MmioRegion::new(
                    bios_base as usize,
                    bios_size,
                    common::MemoryType::Uncacheable,
                )
            } {
                Ok(region) => match Bios::new(region) {
                    Ok(bios) => Some(bios),
                    Err(err) => {
                        log::warn!("failed to parse BIOS at {:08X}: {}", bios_base, err);
                        None
                    }
                },
                Err(err) => {
                    log::warn!("failed to map BIOS at {:08X}: {}", bios_base, err);
                    None
                }
            }
        } else {
            None
        };

        let ggtt = unsafe {
            GlobalGtt::new(
                pcid_handle,
                gttmm.clone(),
                //TODO: how to use 64-bit surface addresses?
                gm.size.min(u32::MAX as usize) as u32,
            )
        };
        //unsafe { ggtt.reset() };

        // GMBUS seems to be stable for all generations
        let gmbus = unsafe { Gmbus::new(&gttmm)? };

        let dpclka_cfgcr0;
        let int;
        let ref_freq;
        match kind {
            DeviceKind::KabyLake => {
                dpclka_cfgcr0 = None;

                int = Interrupter {
                    change_detects: Vec::new(),
                    // IHD-OS-KBL-Vol 2c-1.17 MASTER_INT_CTL
                    display_int_ctl: Interrupter_display_int_ctl {
                        reg: unsafe { gttmm.mmio(0x44200)? },
                        enable: 1 << 31,
                        sde: 1 << 23,
                    },
                    gfx_mstr_intr: None,
                    sde_interrupt: unsafe { InterruptRegs::new(&gttmm, 0xC4000)? },
                };

                // IHD-OS-KBL-Vol 12-1.17
                ref_freq = 24_000_000;
            }
            DeviceKind::TigerLake
            | DeviceKind::AlderLakeP
            | DeviceKind::Alchemist
            | DeviceKind::MeteorLakeP => {
                // TigerLake: IHD-OS-TGL-Vol 2c-12.21
                //TODO: Alder Lake-P support is from inspecting MIT-licensed DRM driver
                // Alchemist: IHD-OS-ACM-Vol 2c-3.23

                dpclka_cfgcr0 = Some(unsafe { gttmm.mmio(0x164280)? });

                let dssm = unsafe { gttmm.mmio(0x51004)? };
                log::debug!("dssm {:08X}", dssm.read());

                const DSSM_REF_FREQ_24_MHZ: u32 = 0b000 << 29;
                const DSSM_REF_FREQ_19_2_MHZ: u32 = 0b001 << 29;
                const DSSM_REF_FREQ_38_4_MHZ: u32 = 0b010 << 29;
                const DSSM_REF_FREQ_MASK: u32 = 0b111 << 29;
                ref_freq = match dssm.read() & DSSM_REF_FREQ_MASK {
                    DSSM_REF_FREQ_24_MHZ => 24_000_000,
                    DSSM_REF_FREQ_19_2_MHZ => 19_200_000,
                    DSSM_REF_FREQ_38_4_MHZ => 38_400_000,
                    unknown => {
                        log::error!("unknown DSSM reference frequency {}", unknown);
                        return Err(Error::new(EIO));
                    }
                };

                int = Interrupter {
                    change_detects: vec![
                        ChangeDetect::new("de_hpd_interrupt", unsafe { gttmm.mmio(0x44470)? }),
                        //TODO: spurious interrupts: ChangeDetect::new("de_port_interrupt", unsafe { gttmm.mmio(0x44440)? }),
                        ChangeDetect::new("shotplug_ctl_ddi", unsafe { gttmm.mmio(0xC4030)? }),
                        ChangeDetect::new("shotplug_ctl_tc", unsafe { gttmm.mmio(0xC4034)? }),
                        ChangeDetect::new("tbt_hotplug_ctl", unsafe { gttmm.mmio(0x44030)? }),
                        ChangeDetect::new("tc_hotplug_ctl", unsafe { gttmm.mmio(0x44038)? }),
                    ],
                    display_int_ctl: Interrupter_display_int_ctl {
                        reg: unsafe { gttmm.mmio(0x44200)? },
                        enable: 1 << 31,
                        sde: 1 << 23,
                    },
                    gfx_mstr_intr: Some(Interrupter_gfx_mstr_intr {
                        reg: unsafe { gttmm.mmio(0x190010)? },
                        display: 1 << 16,
                        enable: 1 << 31,
                    }),
                    sde_interrupt: unsafe { InterruptRegs::new(&gttmm, 0xC4000)? },
                };
            }
        }

        let buffers;
        let ddis;
        let dplls;
        let pipes;
        let power_wells;
        let transcoders;
        match kind {
            DeviceKind::KabyLake => {
                buffers = 1024;
                ddis = Ddi::kabylake(&gttmm)?;
                //TODO: kaby lake dplls
                dplls = Vec::new();
                pipes = Pipe::kabylake(&gttmm)?;
                power_wells = PowerWells::kabylake(&gttmm)?;
                transcoders = Transcoder::kabylake(&gttmm)?;
            }
            DeviceKind::TigerLake => {
                buffers = 2048;
                ddis = Ddi::tigerlake(&gttmm)?;
                dplls = Dpll::tigerlake(&gttmm)?;
                pipes = Pipe::tigerlake(&gttmm)?;
                power_wells = PowerWells::tigerlake(&gttmm)?;
                transcoders = Transcoder::tigerlake(&gttmm)?;
            }
            //TODO: ensure Alder Lake-P (XE_LPD) and Meteor Lake-P (XE_LPDP) match Alchemist
            DeviceKind::AlderLakeP | DeviceKind::Alchemist | DeviceKind::MeteorLakeP => {
                // Some registers are identical to tigerlake
                buffers = 2048;
                dplls = Dpll::tigerlake(&gttmm)?;
                pipes = Pipe::tigerlake(&gttmm)?;
                transcoders = Transcoder::tigerlake(&gttmm)?;
                // DDIs and power wells are distinct
                ddis = Ddi::alchemist(&gttmm)?;
                power_wells = PowerWells::alchemist(&gttmm)?;
            }
        }

        Ok(Self {
            unique: format!("pci:{}", pcid_handle.config().func.addr),
            kind,
            alloc_buffers: RangeAllocator::new(0..buffers),
            bios,
            ddis,
            dpclka_cfgcr0,
            dplls,
            events: VecDeque::new(),
            int,
            gttmm,
            ggtt,
            gm,
            gmbus,
            pipes,
            power_wells,
            ref_freq,
            transcoders,
        })
    }

    pub fn init_inner(&mut self, objects: &mut KmsObjects<Self>) {
        // Add static objects
        let mut free_crtc_ids = VecDeque::new();
        let mut assigned_crtc_ids = HashMap::new();
        for (transcoder, pipe) in self.transcoders.iter_mut().zip(self.pipes.iter_mut()) {
            let (crtc_id, primary_plane_id) = objects.add_crtc(
                Crtc {
                    transcoder_idx: transcoder.index,
                    pipe_idx: pipe.index,
                },
                (),
                scheme::Plane {
                    pipe_idx: pipe.index,
                    plane_idx: 0,
                },
                (),
                //TODO: cursor plane
                None,
            );
            pipe.planes[0].kms_id = Some(primary_plane_id);
            //TODO: support other planes
            let ddi_select = transcoder.ddi_select();
            if ddi_select == 0 {
                free_crtc_ids.push_back(crtc_id);
            } else {
                if let Some(other_id) = assigned_crtc_ids.insert(ddi_select, crtc_id) {
                    panic!(
                        "ddi select {:#x} used for CRTC {:?} and {:?}",
                        ddi_select, other_id, crtc_id
                    )
                }
            }
        }
        for ddi in self.ddis.iter_mut() {
            //TODO: console-draw and orbital don't work well with multiple CRTCs per connector
            let Some(crtc_id) = ddi
                .trans_ddi_select
                .and_then(|ddi_select| assigned_crtc_ids.get(&ddi_select).cloned())
                .or_else(|| free_crtc_ids.pop_front())
            else {
                log::warn!("no CRTC available for DDI {}", ddi.name);
                continue;
            };
            let connector_id = objects.add_connector(
                Connector {
                    ddi_idx: ddi.index,
                    ddi_name: ddi.name,
                    edid: None,
                },
                (),
                &[crtc_id],
            );
            {
                let mut connector = objects.get_connector(connector_id).unwrap().lock().unwrap();
                connector.connection = KmsConnectorStatus::Unknown;
                connector.state.crtc_id = crtc_id;
            }
            ddi.kms_id = Some(connector_id);
        }

        self.dump();

        // Discover current framebuffers
        for crtc_id in objects.crtc_ids().to_vec() {
            let driver_data = objects.get_crtc(crtc_id).unwrap().driver_data;
            let pipe = &self.pipes[driver_data.pipe_idx];
            let transcoder = &self.transcoders[driver_data.transcoder_idx];
            for plane in pipe.planes.iter() {
                if plane.ctl.read().enable() {
                    plane.fetch_modeset(&mut self.alloc_buffers);

                    let fb = plane.fetch_framebuffer(&self.gm, &mut self.ggtt);
                    log::info!("plane {}{}: {:?}", pipe.name, plane.name, fb);

                    //TODO: use EDID for firmware mode instead of modeinfo_for_size
                    objects
                        .get_crtc(crtc_id)
                        .unwrap()
                        .state
                        .lock()
                        .unwrap()
                        .mode = Some(modeinfo_for_size(fb.width, fb.height));

                    let ddi_select = transcoder.ddi_select();
                    if let Some(ddi) = self
                        .ddis
                        .iter_mut()
                        .find(|ddi| ddi.trans_ddi_select == Some(ddi_select))
                    {
                        let mut connector = objects
                            .get_connector(ddi.kms_id.unwrap())
                            .unwrap()
                            .lock()
                            .unwrap();
                        connector.connection = KmsConnectorStatus::Connected;
                        connector.update_from_size(fb.width, fb.height);
                    }

                    let fb = objects.add_framebuffer(fb);

                    objects
                        .get_plane(plane.kms_id.unwrap())
                        .unwrap()
                        .state
                        .lock()
                        .unwrap()
                        .fb = Some(fb);
                }
            }
        }

        log::info!(
            "device initialized with {} framebuffers",
            objects.fb_ids().len()
        );

        // Enable SDE interrupts
        {
            let mut mask = 0;
            for ddi in self.ddis.iter() {
                if let Some(sde_interrupt_hotplug) = ddi.sde_interrupt_hotplug {
                    mask |= sde_interrupt_hotplug;
                }
            }
            // Enable DDI hotplug interrupts
            self.int.sde_interrupt.enable(mask);
        }
        // Enable pipe vblank interrupts
        for pipe in self.pipes.iter_mut() {
            pipe.interrupt.enable(1);
        }
        // Enable display interrupts
        self.int.display_int_ctl.write(|data| data.set_enable(true));
        if let Some(gfx_mstr_intr) = &mut self.int.gfx_mstr_intr {
            // Enable graphics interrupts
            gfx_mstr_intr.write(|data| data.set_enable(true));
        }
        for change_detect in self.int.change_detects.iter_mut() {
            change_detect.log();
        }
    }

    pub fn dump(&self) {
        for ddi in self.ddis.iter() {
            if ddi.buf_ctl.read().enable() {
                ddi.dump();
            }
        }

        if let Some(dpclka_cfgcr0) = &self.dpclka_cfgcr0 {
            eprintln!("dpclka_cfgcr0 {:08X}", dpclka_cfgcr0.read());
        }
        for dpll in self.dplls.iter() {
            if dpll.is_enabled() {
                dpll.dump();
            }
        }

        for (transcoder, pipe) in self.transcoders.iter().zip(self.pipes.iter()) {
            if transcoder.conf.read().enable() {
                transcoder.dump();
                pipe.dump();
                for plane in pipe.planes.iter() {
                    if plane.index == 0 || plane.ctl.read().enable() {
                        eprint!("  ");
                        plane.dump();
                    }
                }
            }
        }
    }

    pub fn probe_ddi(
        &mut self,
        objects: &mut KmsObjects<Self>,
        connector_id: KmsObjectId,
    ) -> Result<bool> {
        let mut connector = objects.get_connector(connector_id).unwrap().lock().unwrap();
        let ddi = &mut self.ddis[connector.driver_data.ddi_idx];

        //TODO: probing repeatedly is causing loss of EDID information
        if connector.driver_data.edid.is_some() {
            return Ok(true);
        }

        // Enable DDI power well
        //TODO: turn off wells later if not used
        self.power_wells.enable_well_by_ddi(ddi.name)?;

        let Some((source, edid_data)) =
            ddi.probe_edid(&mut self.power_wells, &self.gttmm, &mut self.gmbus)?
        else {
            return Ok(false);
        };

        // Return if EDID all zeroes, reduces logging from parsing errors below
        if edid_data.iter().all(|x| *x == 0) {
            log::debug!(
                "DDI {} failed to read EDID from {}: all zeroes",
                ddi.name,
                source,
            );
            return Ok(false);
        }

        let tags = ExtensionLibrary::with_standard_handlers();
        match piaf::parse_edid_owned(&edid_data, &tags) {
            Ok(edid) => {
                log::info!("DDI {} EDID from {}: {:?}", ddi.name, source, edid);
                connector.driver_data.edid = Some(edid);
                drop(connector);
                objects.set_connector_edid(connector_id, edid_data.to_vec());
                Ok(true)
            }
            Err(err) => {
                log::warn!(
                    "DDI {} failed to parse EDID from {}: {:?}",
                    ddi.name,
                    source,
                    err
                );
                // Will try again but not fail the driver
                Ok(false)
            }
        }
    }

    pub fn modeset_ddi(
        &mut self,
        objects: &KmsObjects<Self>,
        name: &str,
        mode: drm_mode_modeinfo,
    ) -> Result<bool> {
        let Some(ddi) = self.ddis.iter_mut().find(|ddi| ddi.name == name) else {
            log::warn!("DDI {} not found", name);
            return Err(Error::new(EIO));
        };

        let edid_video_input = objects
            .get_connector(ddi.kms_id.unwrap())
            .unwrap()
            .lock()
            .unwrap()
            .driver_data
            .edid
            .as_ref()
            .and_then(|parsed| {
                let library = ExtensionLibrary::with_standard_handlers();
                let caps = capabilities_from_edid(parsed, &library);
                caps.video_interface
            });

        // Enable DDI power well
        //TODO: turn off wells later if not used
        self.power_wells.enable_well_by_ddi(ddi.name)?;

        let mut modeset = |ddi: &mut Ddi, input: VideoInput| -> Result<()> {
            // IHD-OS-TGL-Vol 12-1.22-Rev2.0 "Sequences for HDMI and DVI"

            // Power wells should already be enabled

            //TODO: Type-C needs aux power enabled and max lanes set

            // Enable port PLL without SSC. Not required on Type-C ports
            if let Some(clock_shift) = ddi.dpclka_cfgcr0_clock_shift {
                // Find free DPLL
                let dpll = self
                    .dplls
                    .iter_mut()
                    .find(|dpll| !dpll.is_enabled())
                    .ok_or_else(|| {
                        log::error!("failed to find free DPLL");
                        Error::new(EIO)
                    })?;

                dpll.configure_and_enable(self.ref_freq, mode, input)?;

                // Update DPLL mapping
                if let Some(dpclka_cfgcr0) = &mut self.dpclka_cfgcr0 {
                    const DPCLKA_CFGCR0_CLOCK_MASK: u32 = 0b11;

                    let mut v = dpclka_cfgcr0.read();
                    v &= !(DPCLKA_CFGCR0_CLOCK_MASK << clock_shift);
                    v |= dpll.dpclka_cfgcr0_clock_value << clock_shift;
                    dpclka_cfgcr0.write(v);
                }
            }

            // Enable DDI clock (must be done separately from PLL mapping)
            if let Some(dpclka_cfgcr0) = &mut self.dpclka_cfgcr0 {
                if let Some(clock_off) = ddi.dpclka_cfgcr0_clock_off {
                    dpclka_cfgcr0.writef(clock_off, false);
                }
            }

            // Enable IO power
            //TODO: the request can be shared by multiple DDIs
            //TODO: skip if TBT
            let pwr_well_ctl_ddi_request = ddi.pwr_well_ctl_ddi_request;
            let pwr_well_ctl_ddi_state = ddi.pwr_well_ctl_ddi_state;
            let power_wells = &mut self.power_wells;
            // Enable IO power
            power_wells.ctl_ddi.writef(pwr_well_ctl_ddi_request, true);
            let timeout = Timeout::from_micros(30);
            while !power_wells.ctl_ddi.readf(pwr_well_ctl_ddi_state) {
                timeout.run().map_err(|()| {
                    log::debug!("timeout while requesting DDI {} IO power", ddi.name);
                    Error::new(EIO)
                })?;
            }
            let mut pwr_guard = DropGuard::new(power_wells, |power_wells| {
                // Disable IO power
                power_wells.ctl_ddi.writef(pwr_well_ctl_ddi_request, false);
            });

            //TODO: Type-C DP_MODE

            // Enable planes, pipe, and transcoder
            {
                // Find free transcoder with free pipe
                let mut transcoder_pipe = None;
                for (transcoder, pipe) in self.transcoders.iter_mut().zip(self.pipes.iter_mut()) {
                    if transcoder.conf.read().enable() {
                        continue;
                    }
                    //TODO: how would we know if pipe is in use?
                    transcoder_pipe = Some((transcoder, pipe));
                    break;
                }
                let Some((transcoder, pipe)) = transcoder_pipe else {
                    log::error!("free transcoder and pipe not found");
                    return Err(Error::new(EIO));
                };

                // Enable pipe and transcoder power wells
                //TODO: turn off wells later if not used
                pwr_guard.enable_well_by_pipe(pipe.name)?;
                pwr_guard.enable_well_by_transcoder(transcoder.name)?;

                // Configure transcoder clock select
                if let Some(clock_select) = ddi.trans_clock_select {
                    transcoder
                        .clk_sel
                        .write(|data| data.set_clk_sel(clock_select));
                }

                // Set pipe bottom color to blue for debugging
                pipe.bottom_color.write(0x3FF);

                // Configure and enable planes
                if let Some(plane) = pipe.planes.first_mut() {
                    plane.modeset(&mut self.alloc_buffers)?;
                    // Framebuffer will be set later
                    plane.set_framebuffer(None);
                }

                //TODO: VGA and panel fitter steps?

                // Configure transcoder timings and other pipe and transcoder settings
                transcoder.modeset(pipe, &mode);

                // Configure and enable TRANS_DDI_FUNC_CTL
                transcoder.ddi_func_ctl.write(|mut data| {
                    data = data
                        .set_enable(true)
                        //TODO: allow different bits per color
                        .set_bpc_bpc8()
                        //TODO: correct port width selection
                        .set_port_width_width4();

                    if let Some(ddi_select) = ddi.trans_ddi_select {
                        data = data.set_ddi(ddi_select);
                    }

                    match input {
                        VideoInput::Hdmi => {
                            data = data.set_mode_hdmi();

                            // Set HDMI scrambling and high TMDS char rate based on symbol rate > 340 MHz
                            if mode.clock > 340_000 {
                                data = data.set_hdmi_scrambling(true).set_high_tmds_char_rate(true);
                            }
                        }
                        VideoInput::Dp => {
                            //TODO: MST
                            data = data.set_mode_dp_sst();
                        }
                    }

                    // Sync polarity
                    if (mode.flags & drm_sys::DRM_MODE_FLAG_PVSYNC) != 0 {
                        data = data.set_sync_polarity_vshigh();
                    }
                    if (mode.flags & drm_sys::DRM_MODE_FLAG_PHSYNC) != 0 {
                        data = data.set_sync_polarity_hshigh();
                    }

                    data
                });

                // Configure and enable TRANS_CONF
                transcoder.conf.modify(|data| {
                    // Set mode to progressive
                    data.set_interlaced_mode_pf_pd()
                        // Enable transcoder
                        .set_enable(true)
                });
                //TODO: what is the correct timeout?
                let timeout = Timeout::from_millis(100);
                while !transcoder.conf.read().state() {
                    timeout.run().map_err(|()| {
                        log::error!(
                            "timeout on DDI {} transcoder {} enable",
                            ddi.name,
                            transcoder.name
                        );
                        Error::new(EIO)
                    })?;
                }
            }

            // Enable port
            {
                // Configure voltage swing and related IO settings
                match input {
                    VideoInput::Hdmi => {
                        ddi.voltage_swing_hdmi()?;
                    }
                    VideoInput::Dp => {
                        //TODO ddi.voltage_swing_dp(&self.gttmm)?;
                        log::error!("voltage swing for DP not implemented");
                        return Err(Error::new(EIO));
                    }
                }

                // Configure PORT_CL_DW10 static power down to power up all lanes
                //TODO: only power up required lanes
                if let Some(mut port_cl_dw10) = ddi.port_cl(PortClReg::Dw10) {
                    port_cl_dw10.writef(0b1111 << 4, false);
                }

                // Configure and enable DDI_BUF_CTL
                //TODO: more DDI_BUF_CTL bits?
                ddi.buf_ctl.modify(|data| data.set_enable(true));

                // Wait for DDI_BUF_CTL IDLE = 0, timeout after 500 us
                let timeout = Timeout::from_micros(500);
                while ddi.buf_ctl.read().idle() {
                    timeout.run().map_err(|()| {
                        log::warn!("timeout while waiting for DDI {} active", ddi.name);
                        Error::new(EIO)
                    })?;
                }
            }

            // Keep IO power on if finished
            DropGuard::dismiss(pwr_guard);

            Ok(())
        };

        if ddi.buf_ctl.read().idle() {
            log::info!("DDI {} idle, will attempt mode setting", ddi.name);
            let input = match edid_video_input {
                Some(
                    piaf::VideoInterface::Dvi
                    | piaf::VideoInterface::HdmiA
                    | piaf::VideoInterface::HdmiB,
                ) => VideoInput::Hdmi,
                Some(piaf::VideoInterface::DisplayPort) => VideoInput::Dp,
                Some(other) => {
                    log::warn!("EDID video input {other:?} not supported");
                    return Err(Error::new(EIO));
                }
                None => {
                    log::warn!("EDID video input not detected");
                    return Err(Error::new(EIO));
                }
            };
            //TODO: DisplayPort modeset not complete
            match modeset(ddi, input) {
                Ok(()) => {
                    log::info!("DDI {} modeset {:?} finished", ddi.name, input);
                }
                Err(err) => {
                    log::warn!("DDI {} modeset {:?} failed: {}", ddi.name, input, err);
                    // Will try again but not fail the driver
                    return Ok(false);
                }
            }
        } else {
            //TODO: allow changing modes at runtime
            log::info!("DDI {} already active", ddi.name);
        }

        Ok(true)
    }

    pub fn handle_display_irq(&mut self) -> bool {
        let display_ints = self.int.display_int_ctl.read().set_enable(false);
        if display_ints.raw() != 0 {
            log::debug!("  display ints {:08X}", display_ints.raw());
            if display_ints.sde() {
                let sde_ints = self.int.sde_interrupt.pending();
                log::debug!("    south display engine ints {:08X}", sde_ints);
                for ddi in self.ddis.iter() {
                    if let Some(sde_interrupt_hotplug) = ddi.sde_interrupt_hotplug {
                        if sde_ints & sde_interrupt_hotplug == sde_interrupt_hotplug {
                            self.events.push_back(Event::DdiHotplug(ddi.name));
                        }
                    }
                }
            }
            for pipe in self.pipes.iter_mut() {
                if display_ints.raw() & pipe.display_int_ctl_pending != 0 {
                    let pipe_ints = pipe.interrupt.pending();
                    log::debug!("    pipe {} ints {:08X}", pipe.name, pipe_ints);
                }
            }
            true
        } else {
            false
        }
    }

    pub fn handle_irq(&mut self) -> bool {
        // NOTE: Disabling the master interrupt control for the duration of the interrupt handler
        // is very important to ensure we don't get into situation where we failed to acknowledge
        // all interrupts and no longer get any PCI interrupts.
        let had_irq = if let Some(gfx_mstr_intr) = &mut self.int.gfx_mstr_intr {
            gfx_mstr_intr.write(|data| data);
            let gfx_ints = gfx_mstr_intr.read().set_enable(false);
            let res = if gfx_ints.raw() != 0 {
                log::debug!("gfx ints {:08X}", gfx_ints.raw());

                if gfx_ints.display() {
                    self.handle_display_irq();
                }

                true
            } else {
                false
            };
            let gfx_mstr_intr = self.int.gfx_mstr_intr.as_mut().unwrap();
            gfx_mstr_intr.write(|data| data.set_enable(true));
            res
        } else {
            self.int.display_int_ctl.write(|data| data);
            let res = self.handle_display_irq();
            self.int.display_int_ctl.write(|data| data.set_enable(true));
            res
        };

        if had_irq {
            for change_detect in self.int.change_detects.iter_mut() {
                change_detect.check();
            }
        }

        had_irq
    }

    pub fn handle_events(scheme: &mut GraphicsScheme<Self>) {
        while let Some(event) = scheme.adapter_mut().events.pop_front() {
            match event {
                Event::DdiHotplug(ddi_name) => {
                    log::info!("DDI {} plugged", ddi_name);

                    let Some(ddi) = scheme
                        .adapter()
                        .ddis
                        .iter()
                        .find(|ddi| ddi.name == ddi_name)
                    else {
                        log::warn!("DDI {} not found", ddi_name);
                        continue;
                    };

                    let connector = ddi.kms_id.unwrap();

                    if let KmsConnectorStatus::Connected = scheme
                        .kms_objects()
                        .get_connector(connector)
                        .unwrap()
                        .lock()
                        .unwrap()
                        .connection
                    {
                        // Avoid surfacing spurious hotplug events to userspace.
                        // FIXME handle disconnects
                        continue;
                    }

                    scheme.notify_connector_hotplug(connector);
                }
            }
        }
    }
}

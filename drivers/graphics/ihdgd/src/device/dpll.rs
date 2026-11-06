use std::mem::DropGuard;

use common::io::{Io, MmioPtr};
use common::timeout::Timeout;
use drm_sys::drm_mode_modeinfo;
use ihdgd_macros::define_regs;
use syscall::error::{Error, Result, EIO};

use super::{MmioRegion, VideoInput};

// FIXME hard code in the define_regs! invocation
pub const DPLL_CFGCR1_QDIV_RATIO_SHIFT: u32 = 10;
pub const DPLL_CFGCR1_QDIV_RATIO_MASK: u32 = 0xFF << DPLL_CFGCR1_QDIV_RATIO_SHIFT;
pub const DPLL_CFGCR1_QDIV_MODE: u32 = 1 << 9;
pub const DPLL_CFGCR1_KDIV_1: u32 = 0b001 << 6;
pub const DPLL_CFGCR1_KDIV_2: u32 = 0b010 << 6;
pub const DPLL_CFGCR1_KDIV_3: u32 = 0b100 << 6;
pub const DPLL_CFGCR1_KDIV_MASK: u32 = 0b111 << 6;
pub const DPLL_CFGCR1_PDIV_2: u32 = 0b0001 << 2;
pub const DPLL_CFGCR1_PDIV_3: u32 = 0b0010 << 2;
pub const DPLL_CFGCR1_PDIV_5: u32 = 0b0100 << 2;
pub const DPLL_CFGCR1_PDIV_7: u32 = 0b1000 << 2;
pub const DPLL_CFGCR1_PDIV_MASK: u32 = 0b1111 << 2;

define_regs! {
    pub struct Dpll {
        let name: &'static str,
        // IHD-OS-TGL-Vol 2c-12.21 DPLL_CFGCR0
        reg cfgcr0: u32,
        // IHD-OS-TGL-Vol 2c-12.21 DPLL_CFGCR1
        reg cfgcr1: u32,
        // IHD-OS-TGL-Vol 2c-12.21 DPLL_DIV0
        reg div0: u32,
        // IHD-OS-TGL-Vol 2c-12.21 DPCLKA_CFGCR0
        pub let dpclka_cfgcr0_clock_value: u32,
        // IHD-OS-TGL-Vol 2c-12.21 DPLL_ENABLE
        reg enable: u32 {
            flag enable = 1 << 31,
            flag lock = 1 << 30,
            flag power_enable = 1 << 27,
            flag power_state = 1 << 26,
        },
        // IHD-OS-TGL-Vol 2c-12.21 DPLL_SSC
        reg ssc: u32 {
            flag enable = 1 << 9,
        },
    }
}

//TODO: verify offsets and count using DeviceKind?
impl Dpll {
    pub fn is_enabled(&self) -> bool {
        self.enable.read().enable()
    }

    pub fn dump(&self) {
        eprint!("Dpll {}", self.name);
        eprint!(" cfgcr0 {:08X}", self.cfgcr0.read());
        eprint!(" cfgcr1 {:08X}", self.cfgcr1.read());
        eprint!(" div0 {:08X}", self.div0.read());
        eprint!(" enable {:08X}", self.enable.read().raw());
        eprint!(" ssc {:08X}", self.ssc.read().raw());
        eprintln!();
    }

    pub fn configure_and_enable(
        &mut self,
        ref_freq: u64,
        mode: drm_mode_modeinfo,
        input: VideoInput,
    ) -> Result<()> {
        // Enable DPLL power
        self.enable.modify(|data| data.set_power_enable(true));
        //TODO: timeout not specified in docs, should be very fast
        let timeout = Timeout::from_micros(1);
        while !self.enable.read().power_state() {
            timeout.run().map_err(|()| {
                log::debug!("timeout while enabling DPLL {} power", self.name);
                Error::new(EIO)
            })?;
        }
        let mut this = DropGuard::new(self, |this| {
            // Disable DPLL power
            this.enable.modify(|data| data.set_power_enable(false));
        });

        match input {
            VideoInput::Hdmi => {
                // Set SSC enable/disable. For HDMI, always disable
                this.ssc.modify(|data| data.set_enable(false));

                // Configure DPLL frequency
                this.set_freq_hdmi(ref_freq, &mode)?;
            }
            VideoInput::Dp => {
                log::warn!("DPLL for DisplayPort not implemented");
                return Err(Error::new(EIO));
            }
        }

        //TODO: "Sequence Before Frequency Change"

        // Enable DPLL
        //TODO: use guard?
        {
            this.enable.modify(|data| data.set_enable(true));
            let timeout = Timeout::from_micros(50);
            while !this.enable.read().lock() {
                timeout.run().map_err(|()| {
                    log::debug!("timeout while enabling DPLL {}", this.name);
                    Error::new(EIO)
                })?;
            }
        }

        //TODO: "Sequence After Frequency Change"

        // Continue to allow DPLL power
        DropGuard::dismiss(this);

        Ok(())
    }

    pub fn set_freq_hdmi(&mut self, mut ref_freq: u64, mode: &drm_mode_modeinfo) -> Result<()> {
        // IHD-OS-TGL-Vol 12-1.22-Rev2.0 "Formula for HDMI Mode DPLL Programming"
        const KHZ: u64 = 1_000;
        const MHZ: u64 = KHZ * 1_000;
        let dco_min: u64 = 7_998 * MHZ;
        let dco_mid: u64 = 8_999 * MHZ;
        let dco_max: u64 = 10_000 * MHZ;

        // If reference frequency is 38.4, use 19.2 because the DPLL automatically divides that by 2.
        if ref_freq == 38_400_000 {
            ref_freq /= 2;
        }

        //TODO: this symbol frequency is only valid for RGB 8 bits per color
        let symbol_freq = u64::from(mode.clock) * KHZ;
        let pll_freq = symbol_freq * 5;

        #[derive(Debug)]
        struct Setting {
            pdiv: u64,
            kdiv: u64,
            qdiv: u64,
            cfgcr1: u32,
            dco: u64,
            dco_dist: u64,
        }

        let mut best_setting: Option<Setting> = None;
        for (pdiv, pdiv_reg) in [
            (2, DPLL_CFGCR1_PDIV_2),
            (3, DPLL_CFGCR1_PDIV_3),
            (5, DPLL_CFGCR1_PDIV_5),
            (7, DPLL_CFGCR1_PDIV_7),
        ] {
            for (kdiv, kdiv_reg) in [
                (1, DPLL_CFGCR1_KDIV_1),
                (2, DPLL_CFGCR1_KDIV_2),
                (3, DPLL_CFGCR1_KDIV_3),
            ] {
                let qdiv_range = if kdiv == 2 { 1..=0xFF } else { 1..=1 };
                for qdiv in qdiv_range {
                    let qdiv_reg = if qdiv == 1 {
                        0
                    } else {
                        ((qdiv as u32) << DPLL_CFGCR1_QDIV_RATIO_SHIFT) | DPLL_CFGCR1_QDIV_MODE
                    };

                    let dco = pll_freq * pdiv * kdiv * qdiv;
                    if dco <= dco_min || dco >= dco_max {
                        // DCO outside of valid range
                        continue;
                    }

                    let dco_dist = dco.abs_diff(dco_mid);

                    let setting = Setting {
                        pdiv,
                        kdiv,
                        qdiv,
                        cfgcr1: pdiv_reg | kdiv_reg | qdiv_reg,
                        dco,
                        dco_dist,
                    };

                    best_setting = match best_setting.take() {
                        Some(other) if other.dco_dist < setting.dco_dist => Some(other),
                        _ => Some(setting),
                    };
                }
            }
        }

        let Some(setting) = best_setting else {
            log::error!("failed to find valid DPLL setting");
            return Err(Error::new(EIO));
        };

        eprintln!("{:?}", setting);

        // Configure DPLL_CFGCR0 to set DCO frequency
        {
            let dco_int = setting.dco / ref_freq;
            let dco_fract = ((setting.dco - (dco_int * ref_freq)) << 15) / ref_freq;
            self.cfgcr0
                .write(((dco_fract as u32) << 10) | (dco_int as u32));
        }

        // Configure DPLL_CFGCR1 to set the dividers
        {
            let mut v = self.cfgcr1.read();
            let mask = DPLL_CFGCR1_QDIV_RATIO_MASK
                | DPLL_CFGCR1_QDIV_MODE
                | DPLL_CFGCR1_KDIV_MASK
                | DPLL_CFGCR1_PDIV_MASK;
            v &= !mask;
            v |= setting.cfgcr1 & mask;
            self.cfgcr1.write(v);
        }

        // Read back DPLL_CFGCR0 and DPLL_CFGCR1 to ensure writes are complete
        let _ = self.cfgcr0.read();
        let _ = self.cfgcr1.read();

        Ok(())
    }

    pub fn tigerlake(gttmm: &MmioRegion) -> Result<Vec<Self>> {
        let mut dplls = Vec::new();
        dplls.push(Self {
            name: "0",
            cfgcr0: unsafe { gttmm.mmio(0x164284)? },
            cfgcr1: unsafe { gttmm.mmio(0x164288)? },
            div0: unsafe { gttmm.mmio(0x164B00)? },
            dpclka_cfgcr0_clock_value: 0b00,
            enable: Dpll_enable {
                reg: unsafe { gttmm.mmio(0x46010)? },
            },
            ssc: Dpll_ssc {
                reg: unsafe { gttmm.mmio(0x164B10)? },
            },
        });
        dplls.push(Self {
            name: "1",
            cfgcr0: unsafe { gttmm.mmio(0x16428C)? },
            cfgcr1: unsafe { gttmm.mmio(0x164290)? },
            div0: unsafe { gttmm.mmio(0x164C00)? },
            dpclka_cfgcr0_clock_value: 0b01,
            enable: Dpll_enable {
                reg: unsafe { gttmm.mmio(0x46014)? },
            },
            ssc: Dpll_ssc {
                reg: unsafe { gttmm.mmio(0x164C10)? },
            },
        });
        /*TODO: not present on U-class CPUs
        dplls.push(Self {
            name: "4",
            cfgcr0: unsafe { gttmm.mmio(0x164294)? },
            cfgcr1: unsafe { gttmm.mmio(0x164298)? },
            div0: unsafe { gttmm.mmio(0x164E00)? },
            dpclka_cfgcr0_clock_value: 0b10,
            enable: unsafe { gttmm.mmio(0x46018)? },
            ssc: unsafe { gttmm.mmio(0x164E10)? },
        });
        */
        Ok(dplls)
    }
}

use common::io::{Io, MmioPtr};
use drm_sys::drm_mode_modeinfo;
use ihdgd_macros::define_regs;
use syscall::error::Result;

use super::{MmioRegion, Pipe};
define_regs! {
    pub struct Transcoder {
        pub let name: &'static str,
        pub let index: usize,
        pub reg clk_sel: u32 {
            field clk_sel,
        },
        // IHD-OS-KBL-Vol 2c-1.17 TRANS_CONF
        // IHD-OS-TGL-Vol 2c-12.21 TRANS_CONF
        pub reg conf: u32 {
            flag enable = 1 << 31,
            flag state = 1 << 30,
            enum interlaced_mode = 0b11 << 21 {
                pf_pd = 0b00,
                pf_id = 0b01,
                if_id = 0b11,
            },
        },
        // IHD-OS-KBL-Vol 2c-1.17 TRANS_DDI_FUNC_CTL
        // IHD-OS-TGL-Vol 2c-12.21 TRANS_DDI_FUNC_CTL
        pub reg ddi_func_ctl: u32 {
            flag enable = 1 << 31,
            field ddi,
            enum mode = 0b111 << 24 {
                hdmi = 0b000,
                dvi = 0b001,
                dp_sst = 0b010,
                dp_mst = 0b011,
            },
            enum bpc = 0b111 << 20 {
                bpc8 = 0b000,
                bpc10 = 0b001,
                bpc6 = 0b010,
                bpc12 = 0b011,
            },
            enum sync_polarity = 0b11 << 16 {
                hshigh = 0b01,
                vshigh = 0b10,
            },
            flag high_tmds_char_rate,
            enum port_width = 0b111 << 1 {
                width1 = 0b000,
                width2 = 0b001,
                width3 = 0b010,
                width4 = 0b011,
            },
            flag hdmi_scrambling,
        },
        pub reg ddi_func_ctl2?: u32,
        pub reg hblank: u32,
        pub reg hsync: u32,
        pub reg htotal: u32,
        pub reg msa_misc: u32,
        pub reg mult: u32,
        pub reg push?: u32,
        pub reg space: u32,
        pub reg stereo3d_ctl: u32,
        pub reg vblank: u32,
        pub reg vrr_ctl?: u32,
        pub reg vrr_flipline?: u32,
        pub reg vrr_status?: u32,
        pub reg vrr_status2?: u32,
        pub reg vrr_vmax?: u32,
        pub reg vrr_vmaxshift?: u32,
        pub reg vrr_vmin?: u32,
        pub reg vrr_vtotal_prev?: u32,
        pub reg vsync: u32,
        pub reg vsyncshift: u32,
        pub reg vtotal: u32,
    }
}

impl Transcoder {
    pub fn dump(&self) {
        eprint!("Transcoder {} {}", self.name, self.index);
        eprint!(" clk_sel {:08X}", self.clk_sel.read().raw());
        eprint!(" conf {:08X}", self.conf.read().raw());
        eprint!(" ddi_func_ctl {:08X}", self.ddi_func_ctl.read().raw());
        if let Some(reg) = &self.ddi_func_ctl2 {
            eprint!(" ddi_func_ctl2 {:08X}", reg.read());
        }
        eprint!(" hblank {:08X}", self.hblank.read());
        eprint!(" hsync {:08X}", self.hsync.read());
        eprint!(" htotal {:08X}", self.htotal.read());
        eprint!(" msa_misc {:08X}", self.msa_misc.read());
        eprint!(" mult {:08X}", self.mult.read());
        if let Some(reg) = &self.push {
            eprint!(" push {:08X}", reg.read());
        }
        eprint!(" space {:08X}", self.space.read());
        eprint!(" stereo3d_ctl {:08X}", self.stereo3d_ctl.read());
        eprint!(" vblank {:08X}", self.vblank.read());
        if let Some(reg) = &self.vrr_ctl {
            eprint!(" vrr_ctl {:08X}", reg.read());
        }
        if let Some(reg) = &self.vrr_flipline {
            eprint!(" vrr_flipline {:08X}", reg.read());
        }
        if let Some(reg) = &self.vrr_status {
            eprint!(" vrr_status {:08X}", reg.read());
        }
        if let Some(reg) = &self.vrr_status2 {
            eprint!(" vrr_status2 {:08X}", reg.read());
        }
        if let Some(reg) = &self.vrr_vmax {
            eprint!(" vrr_vmax {:08X}", reg.read());
        }
        if let Some(reg) = &self.vrr_vmaxshift {
            eprint!(" vrr_vmaxshift {:08X}", reg.read());
        }
        if let Some(reg) = &self.vrr_vmin {
            eprint!(" vrr_vmin {:08X}", reg.read());
        }
        if let Some(reg) = &self.vrr_vtotal_prev {
            eprint!(" vrr_vtotal_prev {:08X}", reg.read());
        }
        eprint!(" vsync {:08X}", self.vsync.read());
        eprint!(" vsyncshift {:08X}", self.vsyncshift.read());
        eprint!(" vtotal {:08X}", self.vtotal.read());
        eprintln!();
    }

    pub fn ddi_select(&self) -> u32 {
        self.ddi_func_ctl.read().ddi()
    }

    pub fn modeset(&mut self, pipe: &mut Pipe, mode: &drm_mode_modeinfo) {
        let hactive = u32::from(mode.hdisplay - 1);
        let htotal = u32::from(mode.htotal - 1);
        let hsync_start = u32::from(mode.hsync_start - 1);
        let hsync_end = u32::from(mode.hsync_end - 1);
        let vactive = u32::from(mode.vdisplay - 1);
        let vtotal = u32::from(mode.vtotal - 1);
        let vsync_start = u32::from(mode.vsync_start - 1);
        let vsync_end = u32::from(mode.vsync_end - 1);

        // Configure horizontal sync
        self.htotal.write(hactive | (htotal << 16));
        self.hblank.write(hactive | (htotal << 16));
        self.hsync.write(hsync_start | (hsync_end << 16));

        // Configure vertical sync
        self.vtotal.write(vactive | (vtotal << 16));
        self.vblank.write(vactive | (vtotal << 16));
        self.vsync.write(vsync_start | (vsync_end << 16));

        // Configure pipe
        pipe.srcsz.write(vactive | (hactive << 16));
    }

    pub fn kabylake(gttmm: &MmioRegion) -> Result<Vec<Self>> {
        let mut transcoders = Vec::with_capacity(4);
        //TODO: Transcoder EDP
        for (i, name) in ["A", "B", "C"].iter().enumerate() {
            transcoders.push(Transcoder {
                name,
                index: i,
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_CLK_SEL
                clk_sel: Transcoder_clk_sel {
                    reg: unsafe { gttmm.mmio(0x46140 + i * 0x4)? },
                    clk_sel_mask: 0b111 << 29,
                    clk_sel_shift: 29,
                },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_CONF
                conf: Transcoder_conf {
                    reg: unsafe { gttmm.mmio(0x70008 + i * 0x1000)? },
                },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_DDI_FUNC_CTL
                ddi_func_ctl: Transcoder_ddi_func_ctl {
                    reg: unsafe { gttmm.mmio(0x60400 + i * 0x1000)? },
                    ddi_mask: 0b111 << 28,
                    ddi_shift: 28,
                    // HDMI scrambling not supported on Kaby Lake
                    hdmi_scrambling: 0,
                    high_tmds_char_rate: 0,
                },
                // N/A
                ddi_func_ctl2: None,
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_HBLANK
                hblank: unsafe { gttmm.mmio(0x60004 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_HSYNC
                hsync: unsafe { gttmm.mmio(0x60008 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_HTOTAL
                htotal: unsafe { gttmm.mmio(0x60000 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_MSA_MISC
                msa_misc: unsafe { gttmm.mmio(0x60410 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_MULT
                mult: unsafe { gttmm.mmio(0x6002C + i * 0x1000)? },
                // N/A
                push: None,
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_SPACE
                space: unsafe { gttmm.mmio(0x60020 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_STEREO3D_CTL
                stereo3d_ctl: unsafe { gttmm.mmio(0x70020 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_VBLANK
                vblank: unsafe { gttmm.mmio(0x60010 + i * 0x1000)? },
                // N/A
                vrr_ctl: None,
                vrr_flipline: None,
                vrr_status: None,
                vrr_status2: None,
                vrr_vmax: None,
                vrr_vmaxshift: None,
                vrr_vmin: None,
                vrr_vtotal_prev: None,
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_VSYNC
                vsync: unsafe { gttmm.mmio(0x60014 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_VSYNCSHIFT
                vsyncshift: unsafe { gttmm.mmio(0x60028 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 TRANS_VTOTAL
                vtotal: unsafe { gttmm.mmio(0x6000C + i * 0x1000)? },
            });
        }
        Ok(transcoders)
    }

    pub fn tigerlake(gttmm: &MmioRegion) -> Result<Vec<Self>> {
        let mut transcoders = Vec::with_capacity(4);
        for (i, name) in ["A", "B", "C", "D"].iter().enumerate() {
            transcoders.push(Transcoder {
                name,
                index: i,
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_CLK_SEL
                clk_sel: Transcoder_clk_sel {
                    reg: unsafe { gttmm.mmio(0x46140 + i * 0x4)? },
                    clk_sel_mask: 0b1111 << 28,
                    clk_sel_shift: 28,
                },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_CONF
                conf: Transcoder_conf {
                    reg: unsafe { gttmm.mmio(0x70008 + i * 0x1000)? },
                },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_DDI_FUNC_CTL
                ddi_func_ctl: Transcoder_ddi_func_ctl {
                    reg: unsafe { gttmm.mmio(0x60400 + i * 0x1000)? },
                    ddi_mask: 0b1111 << 27,
                    ddi_shift: 27,
                    hdmi_scrambling: 1 << 0,
                    high_tmds_char_rate: 1 << 4,
                },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_DDI_FUNC_CTL2
                ddi_func_ctl2: Some(unsafe { gttmm.mmio(0x60404 + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_HBLANK
                hblank: unsafe { gttmm.mmio(0x60004 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_HSYNC
                hsync: unsafe { gttmm.mmio(0x60008 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_HTOTAL
                htotal: unsafe { gttmm.mmio(0x60000 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_MSA_MISC
                msa_misc: unsafe { gttmm.mmio(0x60410 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_MULT
                mult: unsafe { gttmm.mmio(0x6002C + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_PUSH
                push: Some(unsafe { gttmm.mmio(0x60A70 + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_SPACE
                space: unsafe { gttmm.mmio(0x60020 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_STEREO3D_CTL
                stereo3d_ctl: unsafe { gttmm.mmio(0x70020 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VBLANK
                vblank: unsafe { gttmm.mmio(0x60010 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VRR_CTL
                vrr_ctl: Some(unsafe { gttmm.mmio(0x60420 + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VRR_FLIPLINE
                vrr_flipline: Some(unsafe { gttmm.mmio(0x60438 + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VRR_STATUS
                vrr_status: Some(unsafe { gttmm.mmio(0x6042C + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VRR_STATUS2
                vrr_status2: Some(unsafe { gttmm.mmio(0x6043C + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VRR_VMAX
                vrr_vmax: Some(unsafe { gttmm.mmio(0x60424 + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VRR_VMAXSHIFT
                vrr_vmaxshift: Some(unsafe { gttmm.mmio(0x60428 + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VRR_VMIN
                vrr_vmin: Some(unsafe { gttmm.mmio(0x60434 + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VRR_VTOTAL_PREV
                vrr_vtotal_prev: Some(unsafe { gttmm.mmio(0x60480 + i * 0x1000)? }),
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VSYNC
                vsync: unsafe { gttmm.mmio(0x60014 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VSYNCSHIFT
                vsyncshift: unsafe { gttmm.mmio(0x60028 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 TRANS_VTOTAL
                vtotal: unsafe { gttmm.mmio(0x6000C + i * 0x1000)? },
            })
        }
        Ok(transcoders)
    }
}

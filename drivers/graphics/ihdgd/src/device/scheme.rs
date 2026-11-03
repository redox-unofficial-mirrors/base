use driver_graphics::kms::connector::{KmsConnectorDriver, KmsConnectorStatus};
use driver_graphics::kms::crtc::{KmsCrtc, KmsCrtcDriver, KmsCrtcState};
use driver_graphics::kms::objects::{KmsObjectId, KmsObjects};
use driver_graphics::kms::plane::{KmsPlane, KmsPlaneDriver, KmsPlaneState};
use driver_graphics::{Buffer, Damage, DumbBufferConfig, GraphicsAdapter};

use super::buffer::GpuBuffer;
use super::Device;

#[derive(Debug)]
pub struct Connector {
    pub ddi_idx: usize,
    pub ddi_name: &'static str,
    pub edid: Option<piaf::ParsedEdid>,
}

impl KmsConnectorDriver for Connector {
    type State = ();
}

#[derive(Clone, Copy, Debug)]
pub struct Crtc {
    pub transcoder_idx: usize,
    pub pipe_idx: usize,
}

impl KmsCrtcDriver for Crtc {
    type State = ();
}

#[derive(Debug)]
pub struct Plane {
    pub pipe_idx: usize,
    pub plane_idx: usize,
}

impl KmsPlaneDriver for Plane {
    type State = ();
}

impl Buffer for GpuBuffer {
    fn size(&self) -> usize {
        self.size as usize
    }
}

impl GraphicsAdapter for Device {
    type Connector = Connector;
    type Crtc = Crtc;
    type Plane = Plane;

    type Buffer = GpuBuffer;
    type Framebuffer = ();

    fn name(&self) -> &'static [u8] {
        b"ihdgd"
    }

    fn desc(&self) -> &'static [u8] {
        b"Intel HD Graphics"
    }

    fn init(&mut self, objects: &mut KmsObjects<Self>) {
        self.init_inner(objects);
    }

    fn get_unique(&self) -> String {
        self.unique.clone()
    }

    fn min_max_fb_size(&self) -> (u32, u32, u32, u32) {
        (0, 16384, 0, 16384)
    }

    fn dumb_buffer_config(&self) -> Option<DumbBufferConfig> {
        Some(DumbBufferConfig {
            preferred_depth: 24,
            prefer_shadow: true,
        })
    }

    fn cursor_size(&self) -> Option<(u64, u64)> {
        None
    }

    fn probe_connector(
        &mut self,
        objects: &mut KmsObjects<Self>,
        id: KmsObjectId,
    ) -> syscall::Result<()> {
        let ddi_name = objects
            .get_connector(id)?
            .lock()
            .unwrap()
            .driver_data
            .ddi_name;
        log::debug!("probe connector {:?}: DDI {}", id, ddi_name);
        let connection = match self.probe_ddi(objects, id) {
            Ok(true) => KmsConnectorStatus::Connected,
            Ok(false) => {
                log::debug!("timeout probing {}", ddi_name);
                KmsConnectorStatus::Disconnected
            }
            Err(err) => {
                log::warn!("failed to probe {}: {}", ddi_name, err);
                KmsConnectorStatus::Disconnected
            }
        };
        {
            let mut connector = objects.get_connector(id).unwrap().lock().unwrap();
            connector.connection = connection;
        }
        Ok(())
    }

    fn create_dumb_buffer(&mut self, width: u32, height: u32) -> (Self::Buffer, u32) {
        GpuBuffer::alloc_dumb(&self.gm, &mut self.ggtt, width, height).unwrap()
    }

    fn map_dumb_buffer(&mut self, buffer: &Self::Buffer) -> *mut u8 {
        buffer.virt
    }

    fn create_framebuffer(&mut self, _buffer: &Self::Buffer) -> Self::Framebuffer {
        ()
    }

    fn set_crtc(
        &mut self,
        objects: &KmsObjects<Self>,
        crtc: &KmsCrtc<Self>,
        state: KmsCrtcState<Self>,
        connector_ids: &[KmsObjectId],
    ) -> syscall::Result<()> {
        match state.mode {
            Some(mode) => {
                log::debug!(
                    "set crtc {}: {:?} {:?}",
                    crtc.crtc_index,
                    unsafe { std::ffi::CStr::from_ptr(mode.name.as_ptr()) },
                    mode
                );
                let mut ddi_name_opt = None;
                for &connector_id in connector_ids {
                    let (crtc_id, ddi_name) = {
                        let connector_mtx = objects.get_connector(connector_id)?;
                        let connector = connector_mtx.lock().unwrap();
                        (connector.state.crtc_id, connector.driver_data.ddi_name)
                    };
                    let conn_crtc = objects.get_crtc(crtc_id)?;
                    if conn_crtc.crtc_index == crtc.crtc_index {
                        ddi_name_opt = Some(ddi_name);
                        break;
                    }
                }
                if let Some(ddi_name) = ddi_name_opt {
                    self.modeset_ddi(objects, ddi_name, mode)?;
                } else {
                    log::warn!("set crtc {}: could not find DDI", crtc.crtc_index);
                }
            }
            None => {
                log::debug!("set crtc {}: no mode", crtc.crtc_index);
            }
        }

        // FIXME set gamma lut:
        // CSC_MODE: CSC before gamma
        // GAMMA_MODE: 8bit palette
        // PAL_LGC[0..256]: u32::from_be_bytes([0, red >> 8, green >> 8, blue >> 8])

        *crtc.state.lock().unwrap() = state;
        Ok(())
    }

    fn set_plane(
        &mut self,
        _objects: &KmsObjects<Self>,
        plane: &KmsPlane<Self>,
        new_plane_state: KmsPlaneState<Self>,
        _damage: Option<Damage>,
    ) -> syscall::Result<()> {
        if let Some(plane_hw) = self.pipes[plane.driver_data.pipe_idx]
            .planes
            .get_mut(plane.driver_data.plane_idx)
        {
            plane_hw.set_framebuffer(new_plane_state.fb.as_deref());
        }

        *plane.state.lock().unwrap() = new_plane_state;

        Ok(())
    }
}

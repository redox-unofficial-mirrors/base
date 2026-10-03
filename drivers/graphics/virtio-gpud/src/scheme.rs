use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use common::dma::Dma;
use common::sgl;
use driver_graphics::kms::connector::{KmsConnectorDriver, KmsConnectorStatus};
use driver_graphics::kms::crtc::{KmsCrtc, KmsCrtcState};
use driver_graphics::kms::objects::{KmsObjectId, KmsObjects};
use driver_graphics::kms::plane::{KmsPlane, KmsPlaneDriver, KmsPlaneState};
use driver_graphics::{
    Buffer as DrmBuffer, Damage, DumbBufferConfig, GraphicsAdapter, GraphicsScheme,
};
use syscall::PAGE_SIZE;
use virtio_core::spec::{Buffer, ChainBuilder, DescriptorFlags};
use virtio_core::transport::{Error, Queue, Transport};

use crate::*;

impl Into<GpuRect> for Damage {
    fn into(self) -> GpuRect {
        GpuRect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

#[derive(Debug)]
pub struct VirtGpuConnector {
    scanout_id: u32,
}

impl KmsConnectorDriver for VirtGpuConnector {
    type State = ();
}

#[derive(Debug)]
pub struct VirtGpuPlane {
    is_cursor: bool,
}

impl KmsPlaneDriver for VirtGpuPlane {
    type State = ();
}

pub struct VirtGpuFramebuffer {
    unref_tx: Sender<ResourceId>,
    id: ResourceId,
    sgl: sgl::Sgl,
    width: u32,
    height: u32,
}

impl fmt::Debug for VirtGpuFramebuffer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("VirtGpuFramebuffer")
            .field("id", &self.id)
            .field("sgl", &self.sgl)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl DrmBuffer for VirtGpuFramebuffer {
    fn size(&self) -> usize {
        (self.width * self.height * 4) as usize
    }
}

impl Drop for VirtGpuFramebuffer {
    fn drop(&mut self) {
        let _ = self.unref_tx.send(self.id);
    }
}

#[derive(Debug, Clone)]
pub struct Display {
    width: u32,
    height: u32,
    edid: Vec<u8>,
    active_resource: Option<ResourceId>,
}

pub struct VirtGpuAdapter<'a> {
    unique: String,
    pub config: &'a mut GpuConfig,
    control_queue: Arc<Queue>,
    cursor_queue: Arc<Queue>,
    transport: Arc<dyn Transport>,
    has_edid: bool,
    unref_tx: Sender<ResourceId>,
    unref_rx: Receiver<ResourceId>,
    displays: Vec<Display>,
    hidden_cursor: Option<Arc<VirtGpuFramebuffer>>,
}

impl<'a> fmt::Debug for VirtGpuAdapter<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtGpuAdapter")
            .field("displays", &self.displays)
            .finish_non_exhaustive()
    }
}

impl<'a> VirtGpuAdapter<'a> {
    pub async fn update_displays(&mut self) -> Result<(), Error> {
        let display_info = self.get_display_info().await?;
        let raw_displays = &display_info.display_info[..self.config.num_scanouts() as usize];

        self.displays.resize(
            raw_displays.len(),
            Display {
                width: 0,
                height: 0,
                edid: vec![],
                active_resource: None,
            },
        );
        for (i, info) in raw_displays.iter().enumerate() {
            log::info!(
                "virtio-gpu: display {i} ({}x{}px)",
                info.rect.width,
                info.rect.height
            );

            // We must ignore info.enabled. All displays other than the first one are not
            // enabled at startup according to QEMU, yet we can set a framebuffer for them
            // just fine.

            if info.rect.width == 0 || info.rect.height == 0 {
                // QEMU gives all displays other than the first a zero width and height, but trying
                // to attach a zero sized framebuffer to the display will result an error, so
                // default to 640x480px.
                self.displays[i].width = 640;
                self.displays[i].height = 480;
                // If we fake a size, then the EDID would need to be faked as well,
                // but we don't have any code to do that.
                self.has_edid = false;
            } else {
                self.displays[i].width = info.rect.width;
                self.displays[i].height = info.rect.height;
            }

            if self.has_edid {
                let edid = self.get_edid(i as u32).await?;
                self.displays[i].edid = edid.edid[..edid.size as usize].to_vec();
            }
        }

        Ok(())
    }

    async fn send_request<T: Send + 'static>(
        &self,
        request: Dma<T>,
    ) -> Result<Dma<ControlHeader>, Error> {
        let header = Dma::new(ControlHeader::default())?;
        let command = ChainBuilder::new()
            .chain(Buffer::new(request))
            .chain(Buffer::new(header).flags(DescriptorFlags::WRITE_ONLY))
            .build();

        let (_, [_, header]) = self.control_queue.send(command).await.into_parts();
        Ok(header.into_inner())
    }

    async fn send_request_fenced<T: Send + 'static>(
        &self,
        request: Dma<T>,
    ) -> Result<Dma<ControlHeader>, Error> {
        let mut header = Dma::new(ControlHeader::default())?;
        header.flags |= VIRTIO_GPU_FLAG_FENCE;
        let command = ChainBuilder::new()
            .chain(Buffer::new(request))
            .chain(Buffer::new(header).flags(DescriptorFlags::WRITE_ONLY))
            .build();

        let (_, [_, header]) = self.control_queue.send(command).await.into_parts();
        Ok(header.into_inner())
    }

    async fn send_request_cursor<T: Send + 'static>(&self, request: Dma<T>) -> Result<(), Error> {
        let command = ChainBuilder::new().chain(Buffer::new(request)).build();
        self.cursor_queue.send(command).await;
        Ok(())
    }

    pub(crate) async fn unref_all_delayed(&self) {
        while let Ok(resource_id) = self.unref_rx.try_recv() {
            let request = Dma::new(ResourceUnref::new(resource_id)).unwrap();

            let header = Dma::new(ControlHeader::default()).unwrap();
            let command = ChainBuilder::new()
                .chain(Buffer::new(request))
                .chain(Buffer::new(header).flags(DescriptorFlags::WRITE_ONLY))
                .build();

            self.control_queue.send(command).await;
        }
    }

    async fn get_display_info(&self) -> Result<Dma<GetDisplayInfo>, Error> {
        let header = Dma::new(ControlHeader::with_ty(CommandTy::GetDisplayInfo))?;

        let response = Dma::new(GetDisplayInfo::default())?;
        let command = ChainBuilder::new()
            .chain(Buffer::new(header))
            .chain(Buffer::new(response).flags(DescriptorFlags::WRITE_ONLY))
            .build();

        let (_, [_, response]) = self.control_queue.send(command).await.into_parts();
        let response: Dma<GetDisplayInfo> = response.into_inner();
        assert!(response.header.ty == CommandTy::RespOkDisplayInfo);

        Ok(response)
    }

    async fn get_edid(&self, scanout_id: u32) -> Result<Dma<GetEdidResp>, Error> {
        let header = Dma::new(GetEdid::new(scanout_id))?;

        let response = Dma::new(GetEdidResp::new())?;
        let command = ChainBuilder::new()
            .chain(Buffer::new(header))
            .chain(Buffer::new(response).flags(DescriptorFlags::WRITE_ONLY))
            .build();

        let (_, [_, response]) = self.control_queue.send(command).await.into_parts();
        let response: Dma<GetEdidResp> = response.into_inner();
        assert!(response.header.ty == CommandTy::RespOkEdid);

        Ok(response)
    }

    async fn resource_create_2d(&mut self, width: u32, height: u32) -> Result<ResourceId, Error> {
        let res_id = ResourceId::alloc();

        let request = Dma::new(ResourceCreate2d::new(
            res_id,
            ResourceFormat::Bgrx,
            width,
            height,
        ))?;
        let header = self.send_request(request).await?;
        assert_eq!(header.ty, CommandTy::RespOkNodata);
        Ok(res_id)
    }

    async fn resource_attach_backing(
        &mut self,
        res_id: ResourceId,
        sgl: &sgl::Sgl,
    ) -> Result<(), Error> {
        let mut mem_entries = unsafe { Dma::zeroed_slice(sgl.chunks().len())?.assume_init() };
        for (entry, chunk) in mem_entries.iter_mut().zip(sgl.chunks().iter()) {
            *entry = MemEntry {
                address: chunk.phys as u64,
                length: chunk.length.next_multiple_of(PAGE_SIZE) as u32,
                padding: 0,
            };
        }

        let attach_request = Dma::new(AttachBacking::new(res_id, mem_entries.len() as u32))?;
        let header = Dma::new(ControlHeader::default())?;
        let command = ChainBuilder::new()
            .chain(Buffer::new(attach_request))
            .chain(Buffer::new(mem_entries))
            .chain(Buffer::new(header).flags(DescriptorFlags::WRITE_ONLY))
            .build();

        let (_, [_, _, header]) = self.control_queue.send(command).await.into_parts();
        let header: Dma<ControlHeader> = header.into_inner();
        assert_eq!(header.ty, CommandTy::RespOkNodata);
        Ok(())
    }

    async fn create_dumb_buffer_inner(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<(VirtGpuFramebuffer, u32), Error> {
        let bpp = 32;
        let fb_size = width as usize * height as usize * bpp / 8;
        let sgl = sgl::Sgl::new(fb_size)?;

        unsafe {
            core::ptr::write_bytes(sgl.as_ptr() as *mut u8, 255, fb_size);
        }

        // Create a host resource using `VIRTIO_GPU_CMD_RESOURCE_CREATE_2D`.
        let res_id = self.resource_create_2d(width, height).await?;

        // Use the allocated framebuffer from the guest ram, and attach it as backing
        // storage to the resource just created, using `VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING`.
        self.resource_attach_backing(res_id, &sgl).await?;

        Ok((
            VirtGpuFramebuffer {
                unref_tx: self.unref_tx.clone(),
                id: res_id,
                sgl,
                width,
                height,
            },
            width * 4,
        ))
    }

    async fn update_cursor(
        &mut self,
        cursor: &VirtGpuFramebuffer,
        scanout_id: u32,
        x: i32,
        y: i32,
        hot_x: i32,
        hot_y: i32,
    ) {
        //Transfering cursor resource to host
        let transfer_request = Dma::new(XferToHost2d::new(
            cursor.id,
            GpuRect {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
            0,
        ))
        .unwrap();
        let header = self.send_request_fenced(transfer_request).await.unwrap();
        assert_eq!(header.ty, CommandTy::RespOkNodata);

        //Update the cursor position
        self.send_request_cursor(
            Dma::new(UpdateCursor::update_cursor(
                scanout_id, x, y, hot_x, hot_y, cursor.id,
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    }

    async fn move_cursor(&mut self, cursor: &VirtGpuFramebuffer, scanout_id: u32, x: i32, y: i32) {
        self.send_request_cursor(
            Dma::new(MoveCursor::move_cursor(scanout_id, x, y, cursor.id)).unwrap(),
        )
        .await
        .unwrap();
    }

    async fn disable_cursor(&mut self, scanout_id: u32) {
        if self.hidden_cursor.is_none() {
            let (width, height) = (64, 64);
            let (cursor, stride) = self.create_dumb_buffer_inner(width, height).await.unwrap();
            unsafe {
                core::ptr::write_bytes(
                    cursor.sgl.as_ptr() as *mut u8,
                    0,
                    (stride * height) as usize,
                );
            }
            self.hidden_cursor = Some(Arc::new(cursor));
        }
        let hidden_cursor = self.hidden_cursor.as_ref().unwrap().clone();

        self.update_cursor(&hidden_cursor, scanout_id, 0, 0, 0, 0)
            .await;
    }
}

impl<'a> GraphicsAdapter for VirtGpuAdapter<'a> {
    type Connector = VirtGpuConnector;
    type Crtc = ();
    type Plane = VirtGpuPlane;

    type Buffer = VirtGpuFramebuffer;
    type Framebuffer = ();

    fn name(&self) -> &'static [u8] {
        b"virtio-gpud"
    }

    fn desc(&self) -> &'static [u8] {
        b"VirtIO GPU"
    }

    fn init(&mut self, objects: &mut KmsObjects<Self>) {
        futures::executor::block_on(async {
            self.update_displays().await.unwrap();
        });

        for scanout_id in 0..self.config.num_scanouts.get() {
            let (crtc, _primary_plane_id) = objects.add_crtc(
                (),
                (),
                VirtGpuPlane { is_cursor: false },
                (),
                Some((VirtGpuPlane { is_cursor: true }, ())),
            );

            objects.add_connector(VirtGpuConnector { scanout_id }, (), &[crtc]);
        }
    }

    fn get_unique(&self) -> String {
        self.unique.clone()
    }

    fn min_max_fb_size(&self) -> (u32, u32, u32, u32) {
        // Linux probably uses this value because an 8192x8192 framebuffer takes 256MiB,
        // which is the max total VRAM usage allowed by QEMU's virtio-gpu device by default.
        (32, 8192, 32, 8192)
    }

    fn dumb_buffer_config(&self) -> Option<DumbBufferConfig> {
        Some(DumbBufferConfig {
            preferred_depth: 24,
            prefer_shadow: false,
        })
    }

    fn cursor_size(&self) -> Option<(u64, u64)> {
        Some((64, 64))
    }

    fn cursor_plane_needs_hotspot(&self) -> bool {
        true
    }

    fn probe_connector(
        &mut self,
        objects: &mut KmsObjects<Self>,
        id: KmsObjectId,
    ) -> syscall::Result<()> {
        futures::executor::block_on(async {
            let mut connector = objects.get_connector(id)?.lock().unwrap();
            let display = &self.displays[connector.driver_data.scanout_id as usize];

            connector.connection = KmsConnectorStatus::Connected;

            if self.has_edid {
                drop(connector);
                objects.set_connector_edid(id, display.edid.clone());
            } else {
                connector.update_from_size(display.width, display.height);
            }
            Ok(())
        })
    }

    fn create_dumb_buffer(&mut self, width: u32, height: u32) -> (Self::Buffer, u32) {
        futures::executor::block_on(async {
            self.create_dumb_buffer_inner(width, height).await.unwrap()
        })
    }

    fn map_dumb_buffer(&mut self, buffer: &Self::Buffer) -> *mut u8 {
        buffer.sgl.as_ptr()
    }

    fn create_framebuffer(&mut self, _buffer: &Self::Buffer) -> Self::Framebuffer {
        ()
    }

    fn set_crtc(
        &mut self,
        _objects: &KmsObjects<Self>,
        crtc: &KmsCrtc<Self>,
        state: KmsCrtcState<Self>,
        _connector_ids: &[KmsObjectId],
    ) -> syscall::Result<()> {
        *crtc.state.lock().unwrap() = state;
        Ok(())
    }

    fn set_plane(
        &mut self,
        objects: &KmsObjects<Self>,
        plane: &KmsPlane<Self>,
        new_plane_state: KmsPlaneState<Self>,
        damage: Option<Damage>,
    ) -> syscall::Result<()> {
        futures::executor::block_on(async {
            let Some(crtc_id) = new_plane_state.crtc_id else {
                // FIXME disable output?
                return Ok(());
            };

            for connector in objects.connectors() {
                let connector = connector.lock().unwrap();

                if connector.state.crtc_id != crtc_id {
                    continue;
                }

                let scanout_id = connector.driver_data.scanout_id;

                if plane.driver_data.is_cursor {
                    if let Some(new_fb) = &new_plane_state.fb {
                        if damage.map_or(true, |damage| damage.width != 0 && damage.height != 0)
                            || plane.state.lock().unwrap().fb.as_ref() != Some(new_fb)
                        {
                            self.update_cursor(
                                &new_fb.buffer,
                                scanout_id,
                                new_plane_state.crtc_rect.x,
                                new_plane_state.crtc_rect.y,
                                new_plane_state.hotspot.unwrap().0,
                                new_plane_state.hotspot.unwrap().1,
                            )
                            .await;
                        } else {
                            self.move_cursor(
                                &new_fb.buffer,
                                scanout_id,
                                new_plane_state.crtc_rect.x,
                                new_plane_state.crtc_rect.y,
                            )
                            .await;
                        }
                    } else {
                        if plane.state.lock().unwrap().fb.is_some() {
                            self.disable_cursor(scanout_id).await;
                        }
                    }

                    continue;
                }

                let Some(framebuffer) = &new_plane_state.fb else {
                    let scanout_request = Dma::new(SetScanout::new(
                        scanout_id,
                        ResourceId::NONE,
                        GpuRect::new(0, 0, 0, 0),
                    ))
                    .unwrap();
                    let header = self.send_request(scanout_request).await.unwrap();
                    assert_eq!(header.ty, CommandTy::RespOkNodata);
                    self.displays[scanout_id as usize].active_resource = None;
                    return Ok(());
                };

                let req = Dma::new(XferToHost2d::new(
                    framebuffer.buffer.id,
                    GpuRect {
                        x: 0,
                        y: 0,
                        width: framebuffer.width,
                        height: framebuffer.height,
                    },
                    0,
                ))
                .unwrap();
                let header = self.send_request(req).await.unwrap();
                assert_eq!(header.ty, CommandTy::RespOkNodata);

                // FIXME once we support resizing we also need to check that the current and target size match
                if self.displays[scanout_id as usize].active_resource != Some(framebuffer.buffer.id)
                {
                    let scanout_request = Dma::new(SetScanout::new(
                        scanout_id,
                        framebuffer.buffer.id,
                        GpuRect::new(0, 0, framebuffer.width, framebuffer.height),
                    ))
                    .unwrap();
                    let header = self.send_request(scanout_request).await.unwrap();
                    assert_eq!(header.ty, CommandTy::RespOkNodata);
                    self.displays[scanout_id as usize].active_resource =
                        Some(framebuffer.buffer.id);
                }

                let flush = ResourceFlush::new(
                    framebuffer.buffer.id,
                    damage
                        .unwrap_or(Damage {
                            x: 0,
                            y: 0,
                            width: framebuffer.width,
                            height: framebuffer.height,
                        })
                        .into(),
                );
                let header = self.send_request(Dma::new(flush).unwrap()).await.unwrap();
                assert_eq!(header.ty, CommandTy::RespOkNodata);
            }

            *plane.state.lock().unwrap() = new_plane_state;

            Ok(())
        })
    }
}

pub struct GpuScheme {}

impl<'a> GpuScheme {
    pub fn new(
        unique: String,
        config: &'a mut GpuConfig,
        control_queue: Arc<Queue>,
        cursor_queue: Arc<Queue>,
        transport: Arc<dyn Transport>,
        has_edid: bool,
    ) -> Result<GraphicsScheme<VirtGpuAdapter<'a>>, Error> {
        let (unref_tx, unref_rx) = mpsc::channel();
        let adapter = VirtGpuAdapter {
            unique,
            config,
            control_queue,
            cursor_queue,
            transport,
            has_edid,
            unref_tx,
            unref_rx,
            displays: vec![],
            hidden_cursor: None,
        };

        Ok(GraphicsScheme::new(
            adapter,
            "display.virtio-gpu".to_owned(),
            false,
        ))
    }
}

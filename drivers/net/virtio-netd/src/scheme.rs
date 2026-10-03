use std::sync::Arc;

use common::dma::Dma;
use driver_network::NetworkAdapter;
use virtio_core::spec::{Buffer, ChainBuilder, DescriptorFlags};
use virtio_core::transport::Queue;

use crate::{VirtHeader, MAX_BUFFER_LEN};

pub struct VirtioNet {
    mac_address: [u8; 6],

    /// Reciever Queue.
    rx: Arc<Queue>,

    /// Transmiter Queue.
    tx: Arc<Queue>,
}

impl VirtioNet {
    pub fn new(mac_address: [u8; 6], rx: Arc<Queue>, tx: Arc<Queue>) -> Self {
        // Populate all of the `rx_queue` with buffers to maximize performence.
        for _ in 0..rx.descriptor_len() {
            let buffer = unsafe {
                Dma::<[u8]>::zeroed_slice(MAX_BUFFER_LEN)
                    .unwrap()
                    .assume_init()
            };

            let chain = ChainBuilder::new()
                .chain(Buffer::new(buffer).flags(DescriptorFlags::WRITE_ONLY))
                .build();

            rx.post(chain);
        }

        Self {
            mac_address,
            rx,
            tx,
        }
    }

    /// Returns the number of bytes read. Returns `0` if the operation would block.
    fn try_recv(&mut self, target: &mut [u8]) -> usize {
        let header_size = size_of::<VirtHeader>();

        let Some(used) = self.rx.pop_completed() else {
            // The read would block.
            return 0;
        };

        let payload_size = used.written as usize - header_size;

        // XXX: The header and packet are added as one output descriptor to the transmit queue,
        //      and the device is notified of the new entry (see 5.1.5 Device Initialization).
        let buffer = used.buffers[0].owner::<Dma<[u8]>>().unwrap();
        // TODO: Check the header.
        let _header = unsafe { &*(buffer.as_ptr() as *const VirtHeader) };
        let packet = &buffer[header_size..(header_size + payload_size)];

        // Copy the packet into the buffer.
        target[..payload_size].copy_from_slice(&packet);

        // Hand the buffer back to the device for the next packet.
        self.rx.post(used.buffers);
        payload_size
    }
}

impl NetworkAdapter for VirtioNet {
    fn mac_address(&mut self) -> [u8; 6] {
        self.mac_address
    }

    fn available_for_read(&mut self) -> usize {
        self.rx.completed_len()
    }

    fn read_packet(&mut self, buf: &mut [u8]) -> syscall::Result<Option<usize>> {
        let bytes = self.try_recv(buf);

        if bytes != 0 {
            // We read some bytes.
            Ok(Some(bytes))
        } else {
            Ok(None)
        }
    }

    fn write_packet(&mut self, buffer: &[u8]) -> syscall::Result<usize> {
        let header = unsafe { Dma::<VirtHeader>::zeroed()?.assume_init() };

        let mut payload = unsafe { Dma::<[u8]>::zeroed_slice(buffer.len())?.assume_init() };
        payload.copy_from_slice(buffer);

        let chain = ChainBuilder::new()
            .chain(Buffer::new(header))
            .chain(Buffer::new(payload))
            .build();

        futures::executor::block_on(self.tx.send(chain));
        Ok(buffer.len())
    }
}

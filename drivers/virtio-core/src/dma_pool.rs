use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use common::dma::Dma;
use crossbeam_queue::ArrayQueue;
use syscall::PAGE_SIZE;

use crate::spec::DmaRegion;

const SLOT_LEN: usize = PAGE_SIZE;

pub struct DmaPool {
    free: ArrayQueue<Dma<[u8]>>,
}

/// A page leased from a `DmaPool`. Dropping it puts the page back,
/// and it keeps the pool alive until then.
pub struct PooledPage {
    page: Option<Dma<[u8]>>,
    pool: Arc<DmaPool>,
}

impl DmaPool {
    pub fn new(slots: usize) -> Result<Arc<Self>, libredox::error::Error> {
        assert!(
            slots > 0,
            "virtio-core: DMA pool slot count must be non-zero"
        );

        let free = ArrayQueue::new(slots);
        for _ in 0..slots {
            let dma = Dma::<[u8]>::zeroed_slice(SLOT_LEN)?;
            // SAFETY: a slice of zeros is a valid slice of u8.
            let page = unsafe { dma.assume_init() };
            free.push(page)
                .unwrap_or_else(|_| panic!("virtio-core: DMA pool free list was full"));
        }

        Ok(Arc::new(Self { free }))
    }

    pub fn lease(self: &Arc<Self>) -> Option<PooledPage> {
        Some(PooledPage {
            page: Some(self.free.pop()?),
            pool: Arc::clone(self),
        })
    }
}

impl Drop for PooledPage {
    fn drop(&mut self) {
        let page = self
            .page
            .take()
            .expect("virtio-core: pool page dropped twice");
        self.pool
            .free
            .push(page)
            .unwrap_or_else(|_| panic!("virtio-core: DMA pool free list was full"));
    }
}

impl PooledPage {
    fn page(&self) -> &Dma<[u8]> {
        self.page.as_ref().expect("virtio-core: pool page is gone")
    }
}

// SAFETY: the page stays mapped at this address, for this many bytes, until the page is dropped.
unsafe impl DmaRegion for PooledPage {
    fn physical(&self) -> usize {
        self.page().physical()
    }

    fn byte_len(&self) -> usize {
        self.page().len()
    }
}

impl Deref for PooledPage {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.page()
    }
}

impl DerefMut for PooledPage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.page.as_mut().expect("virtio-core: pool page is gone")
    }
}

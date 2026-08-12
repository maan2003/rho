use std::fmt;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// A DMA-BUF format proven importable by GPUI's selected Vulkan device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub struct LinuxDmaBufFormat {
    pub fourcc: u32,
    pub modifier: u64,
}

/// The DRM device and formats used by GPUI's Vulkan device.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct LinuxDmaBufDevice {
    pub render_node: PathBuf,
    pub device_id: u64,
    pub formats: Arc<[LinuxDmaBufFormat]>,
}

static LINUX_DMABUF_DEVICE: OnceLock<RwLock<Option<LinuxDmaBufDevice>>> = OnceLock::new();

/// Returns the DMA-BUF capabilities of GPUI's selected Vulkan device.
pub fn linux_dmabuf_device() -> Option<LinuxDmaBufDevice> {
    LINUX_DMABUF_DEVICE
        .get_or_init(Default::default)
        .read()
        .unwrap()
        .clone()
}

/// Installs the capabilities of GPUI's process-wide WGPU device.
pub fn set_linux_dmabuf_device(device: LinuxDmaBufDevice) {
    *LINUX_DMABUF_DEVICE
        .get_or_init(Default::default)
        .write()
        .unwrap() = Some(device);
}

/// Clears DMA-BUF capabilities while GPUI changes or loses its device.
#[doc(hidden)]
pub fn clear_linux_dmabuf_device() {
    *LINUX_DMABUF_DEVICE
        .get_or_init(Default::default)
        .write()
        .unwrap() = None;
}

/// A single-plane DMA-BUF commit with an explicit acquire fence.
#[derive(Clone)]
pub struct LinuxDmaBufSurface(Arc<LinuxDmaBufSurfaceInner>);

struct LinuxDmaBufSurfaceInner {
    pub id: u64,
    pub lease_id: u64,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    pub y_inverted: bool,
    pub fd: Mutex<Option<OwnedFd>>,
    pub acquire_fence: Mutex<Option<OwnedFd>>,
    pub submitted: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    pub released: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[allow(missing_docs)]
impl LinuxDmaBufSurface {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
        y_inverted: bool,
        fd: OwnedFd,
        acquire_fence: OwnedFd,
        submitted: impl FnOnce() + Send + 'static,
        released: impl FnOnce() + Send + 'static,
    ) -> Self {
        static NEXT_LEASE_ID: AtomicU64 = AtomicU64::new(1);
        Self(Arc::new(LinuxDmaBufSurfaceInner {
            id,
            lease_id: NEXT_LEASE_ID.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            fourcc,
            modifier,
            stride,
            offset,
            y_inverted,
            fd: Mutex::new(Some(fd)),
            acquire_fence: Mutex::new(Some(acquire_fence)),
            submitted: Mutex::new(Some(Box::new(submitted))),
            released: Mutex::new(Some(Box::new(released))),
        }))
    }

    pub fn id(&self) -> u64 {
        self.0.id
    }
    #[doc(hidden)]
    pub fn lease_id(&self) -> u64 {
        self.0.lease_id
    }
    pub fn width(&self) -> u32 {
        self.0.width
    }
    pub fn height(&self) -> u32 {
        self.0.height
    }
    pub fn fourcc(&self) -> u32 {
        self.0.fourcc
    }
    pub fn modifier(&self) -> u64 {
        self.0.modifier
    }
    pub fn stride(&self) -> u32 {
        self.0.stride
    }
    pub fn offset(&self) -> u32 {
        self.0.offset
    }
    pub fn y_inverted(&self) -> bool {
        self.0.y_inverted
    }
    #[doc(hidden)]
    pub fn take_fd(&self) -> Option<OwnedFd> {
        self.0.fd.lock().unwrap().take()
    }
    #[doc(hidden)]
    pub fn take_acquire_fence(&self) -> Option<OwnedFd> {
        self.0.acquire_fence.lock().unwrap().take()
    }
    #[doc(hidden)]
    pub fn submitted(&self) {
        if let Some(callback) = self.0.submitted.lock().unwrap().take() {
            callback();
        }
    }
    #[doc(hidden)]
    pub fn released(&self) {
        if let Some(callback) = self.0.released.lock().unwrap().take() {
            callback();
        }
    }
}

impl Drop for LinuxDmaBufSurfaceInner {
    fn drop(&mut self) {
        if let Some(callback) = self.released.get_mut().unwrap().take() {
            callback();
        }
    }
}

impl PartialEq for LinuxDmaBufSurface {
    fn eq(&self, other: &Self) -> bool {
        self.lease_id() == other.lease_id()
    }
}
impl Eq for LinuxDmaBufSurface {}
impl fmt::Debug for LinuxDmaBufSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinuxDmaBufSurface")
            .field("id", &self.id())
            .field("size", &(self.width(), self.height()))
            .field("fourcc", &self.fourcc())
            .field("modifier", &self.modifier())
            .finish()
    }
}

use std::fmt;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

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
    route_owner: AtomicU64,
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
            route_owner: AtomicU64::new(0),
        }))
    }

    pub fn id(&self) -> u64 {
        self.0.id
    }
    #[doc(hidden)]
    pub fn lease_id(&self) -> u64 {
        self.0.lease_id
    }
    /// Returns whether the renderer is the sole owner of this imported lease.
    ///
    /// An owner outside the renderer means the surface can return to a future
    /// scene. Its DMA-BUF and acquire fence are one-shot, so the renderer must
    /// retain that import rather than attempting to import it again later.
    #[doc(hidden)]
    pub fn renderer_is_sole_owner(&self) -> bool {
        Arc::strong_count(&self.0) == 1
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
    /// Takes the one-shot import handles without partially consuming them.
    #[doc(hidden)]
    pub fn take_import_payload(&self) -> Option<(OwnedFd, OwnedFd)> {
        let mut fd = self.0.fd.lock().unwrap();
        let mut fence = self.0.acquire_fence.lock().unwrap();
        if fd.is_none() || fence.is_none() {
            return None;
        }
        if self
            .0
            .route_owner
            .compare_exchange(0, u64::MAX, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        Some((fd.take().unwrap(), fence.take().unwrap()))
    }
    /// Claim this lease for one Wayland passthrough surface. Repeated commits
    /// by the same owner are allowed; WGPU and other child surfaces are not.
    #[doc(hidden)]
    pub fn claim_wayland_passthrough(&self, owner: u64) -> bool {
        debug_assert_ne!(owner, 0);
        debug_assert_ne!(owner, u64::MAX);
        match self
            .0
            .route_owner
            .compare_exchange(0, owner, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(current) => current == owner,
        }
    }
    /// Whether this lease is unclaimed or already owned by WGPU.
    #[doc(hidden)]
    pub fn texture_route_available(&self) -> bool {
        matches!(self.0.route_owner.load(Ordering::Acquire), 0 | u64::MAX)
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

/// One plane of a DMA-BUF imported by a compositor-owned Wayland child.
#[derive(Debug)]
#[allow(missing_docs)]
pub struct LinuxWaylandDmaBufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// A DMA-BUF commit for a compositor-owned Wayland child surface.
#[allow(missing_docs)]
pub struct LinuxWaylandPassthroughBuffer {
    pub surface: LinuxDmaBufSurface,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub y_inverted: bool,
    pub planes: Vec<LinuxWaylandDmaBufPlane>,
    pub acquire_fence: OwnedFd,
}

/// Host-compositor feedback for one child-surface commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum LinuxWaylandPassthroughEvent {
    Frame {
        scene_id: u64,
        callback_time: u32,
    },
    Presented {
        scene_id: u64,
        timestamp: Duration,
        refresh: Duration,
        sequence: u64,
        flags: u32,
    },
    Discarded {
        scene_id: u64,
    },
}

/// A below-parent Wayland child used to present DMA-BUFs without sampling them
/// through GPUI's renderer.
pub trait LinuxWaylandPassthrough: Send + Sync {
    /// Update logical child geometry. Completion means the position request was
    /// sent before the caller's following parent-surface commit.
    fn set_geometry(&self, bounds: crate::Bounds<crate::Pixels>) -> anyhow::Result<()>;
    /// Attach and commit a new DMA-BUF scene.
    fn present(&self, scene_id: u64, buffer: LinuxWaylandPassthroughBuffer) -> anyhow::Result<()>;
    /// Unmap the child. Already committed buffers retain their release owner.
    fn hide(&self);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn surface(releases: Arc<AtomicUsize>) -> LinuxDmaBufSurface {
        LinuxDmaBufSurface::new(
            1,
            1,
            1,
            0,
            0,
            4,
            0,
            false,
            std::fs::File::open("/dev/null").unwrap().into(),
            std::fs::File::open("/dev/null").unwrap().into(),
            || {},
            move || {
                releases.fetch_add(1, Ordering::SeqCst);
            },
        )
    }

    #[test]
    fn dma_buf_lease_has_one_presentation_route_and_one_release() {
        let releases = Arc::new(AtomicUsize::new(0));
        let passthrough = surface(releases.clone());
        assert!(passthrough.claim_wayland_passthrough(7));
        assert!(passthrough.claim_wayland_passthrough(7));
        assert!(!passthrough.claim_wayland_passthrough(8));
        assert!(passthrough.take_import_payload().is_none());
        passthrough.released();
        passthrough.released();
        assert_eq!(releases.load(Ordering::SeqCst), 1);

        let texture = surface(Arc::new(AtomicUsize::new(0)));
        assert!(texture.take_import_payload().is_some());
        assert!(!texture.claim_wayland_passthrough(7));
    }
}

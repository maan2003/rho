use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use crate::{Error, sys};

// libvpx と表示側の両方が解放するまで、プールはバッファを再利用しない。
// 書き込みは get() から次の decode() 完了まで libvpx のみに許可する。
pub(crate) struct Buffer(UnsafeCell<Vec<u8>>);
unsafe impl Send for Buffer {}
unsafe impl Sync for Buffer {}

pub(crate) type Pool = Mutex<Vec<Arc<Buffer>>>;

/// デコーダーの寿命を超えて保持できる、コピー不要のフレーム。
#[derive(Clone)]
pub struct RetainedFrame {
    buffer: Arc<Buffer>,
    ranges: [std::ops::Range<usize>; 3],
    strides: [usize; 3],
    width: usize,
    height: usize,
}
impl std::fmt::Debug for RetainedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}
impl RetainedFrame {
    /// プレーンのデータを返す。行の末尾にはパディングがある。
    pub fn plane(&self, index: usize) -> &[u8] {
        // この Arc が存在する間、プールはバッファを書き込み用に貸し出さない。
        &(unsafe { &*self.buffer.0.get() })[self.ranges[index].clone()]
    }
    /// 行ストライドを返す。
    pub fn stride(&self, index: usize) -> usize {
        self.strides[index]
    }
    /// 幅を返す。
    pub fn width(&self) -> usize {
        self.width
    }
    /// 高さを返す。
    pub fn height(&self) -> usize {
        self.height
    }

    pub(crate) unsafe fn from_image(image: &sys::vpx_image) -> Result<Self, Error> {
        let fail = || {
            Error::with_reason(
                sys::vpx_codec_err_t_VPX_CODEC_ERROR,
                "retain_frame",
                "invalid external frame buffer",
            )
        };
        if image.fb_priv.is_null() {
            return Err(fail());
        }
        let buffer = unsafe { &*image.fb_priv.cast::<Arc<Buffer>>() }.clone();
        let bytes = unsafe { &*buffer.0.get() };
        let width = image.d_w as usize;
        let height = image.d_h as usize;
        let mut ranges = std::array::from_fn(|_| 0..0);
        let mut strides = [0; 3];
        for i in 0..3 {
            let rows = if i == 0 {
                height
            } else {
                height.div_ceil(1 << image.y_chroma_shift)
            };
            let stride = usize::try_from(image.stride[i]).map_err(|_| fail())?;
            let start = (image.planes[i] as usize)
                .checked_sub(bytes.as_ptr() as usize)
                .ok_or_else(fail)?;
            let length = rows.checked_mul(stride).ok_or_else(fail)?;
            let end = start
                .checked_add(length)
                .filter(|&end| end <= bytes.len())
                .ok_or_else(fail)?;
            ranges[i] = start..end;
            strides[i] = stride;
        }
        Ok(Self {
            buffer,
            ranges,
            strides,
            width,
            height,
        })
    }
}
pub(crate) unsafe extern "C" fn get(
    private: *mut c_void,
    minimum: usize,
    fb: *mut sys::vpx_codec_frame_buffer_t,
) -> i32 {
    // コールバック内でパニックを起こさず、過大な画像は割り当て前に拒否する。
    if minimum > 256 * 1024 * 1024 {
        return -1;
    }
    let pool = unsafe { &*private.cast::<Pool>() };
    let Ok(mut pool) = pool.lock() else {
        return -1;
    };
    let index = pool
        .iter()
        .position(|buffer| Arc::strong_count(buffer) == 1);
    let buffer = if let Some(index) = index {
        let buffer = Arc::get_mut(&mut pool[index]).expect("pool owns free buffer");
        let data = buffer.0.get_mut();
        if data.len() < minimum {
            data.resize(minimum, 0);
        }
        pool[index].clone()
    } else {
        let buffer = Arc::new(Buffer(UnsafeCell::new(vec![0; minimum])));
        pool.push(buffer.clone());
        buffer
    };
    let data = unsafe { &mut *buffer.0.get() };
    unsafe {
        (*fb).data = data.as_mut_ptr();
        (*fb).size = data.len();
        (*fb).priv_ = Box::into_raw(Box::new(buffer)).cast();
    }
    0
}
pub(crate) unsafe extern "C" fn release(
    _: *mut c_void,
    fb: *mut sys::vpx_codec_frame_buffer_t,
) -> i32 {
    unsafe {
        if !(*fb).priv_.is_null() {
            drop(Box::from_raw((*fb).priv_.cast::<Arc<Buffer>>()));
            (*fb).priv_ = std::ptr::null_mut();
        }
    }
    0
}

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::Memory::{
            FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW,
            UnmapViewOfFile,
        },
    },
    core::PCWSTR,
};

use crate::PhysicsPage;

/// RAII wrapper around a read-only file mapping.
///
/// Opens a named shared-memory object and maps it into the process address
/// space for the lifetime of the value. The view is unmapped and the handle
/// is closed when this struct is dropped.
pub struct MappedView {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    size: usize,
}

impl MappedView {
    /// Opens the named file mapping `name` and maps `size` bytes as read-only.
    ///
    /// # Errors
    ///
    /// Returns a [`windows::core::Error`] if the mapping does not exist or
    /// the view cannot be created (e.g. the game is not running).
    pub fn open(name: PCWSTR, size: usize) -> windows::core::Result<Self> {
        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_READ.0, false, name)?;

            let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, size);

            if view.Value.is_null() {
                let err = windows::core::Error::from_thread();
                let _ = CloseHandle(handle);
                return Err(err);
            }

            Ok(Self { handle, view, size })
        }
    }

    /// Copies `T` out of the mapped region by value.
    ///
    /// # Safety
    ///
    /// - `T` must match the actual layout of the data written by the producer.
    /// - `size_of::<T>()` must be <= the `size` passed to [`MappedView::open`].
    pub unsafe fn read(&self) -> PhysicsPage {
        unsafe { std::ptr::read(self.view.Value.cast()) }
    }

    /// Returns the number of bytes that were mapped.
    pub fn size(&self) -> usize {
        self.size
    }
}

impl Drop for MappedView {
    fn drop(&mut self) {
        // Errors during cleanup cannot be meaningfully handled; ignore them.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

// SAFETY: `MappedView` is the sole owner of its handle and mapped pointer.
// Transferring ownership to another thread is safe because the `read` method
// copies data out by value, so no references into the mapping can escape.
unsafe impl Send for MappedView {}

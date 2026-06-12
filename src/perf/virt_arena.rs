#![allow(unused)]
//! This implementation is based on reserving a large amount of virtual memory and
//! lazily committing physical pages as needed, with 4 byte handles.

// Relevant links:
//
// Linux
// https://www.man7.org/linux/man-pages/man2/mmap.2.html
// https://www.man7.org/linux/man-pages/man2/madvise.2.html
// https://www.man7.org/linux/man-pages/man2/mprotect.2.html
//
// MacOS
// https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/madvise.2.html
//
// Windows
// https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-virtualalloc
// https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-virtualfree
//
// NOTE: Tested w/ and w/out overcommit on Linux 6.17, and on out-of-the-box MacOS.
// *when overcommit was off* PROT_NONE + MADV_FREE was not enough to release phys pages.
// otherwise, mprotect(PROT_NONE) + madvise(MADV_FREE) is sufficient: MacOS immediately freed the memory, while
// Linux w/ overcommit=0 (default, allows overcommit) frees the memory when under memory pressure.
// (re)mapping w/ PROT_NONE is needed to support overcommit off (=2).
// mmap(NORESERVE) does affect commit limit (https://albertnetymk.github.io/2023/09/03/mmap/),
// but PROT_NONE achieved the same effect on my Linux machine.

// TODO: branded lifetimes to make unchecked resolve sound
use core::{marker::PhantomData, num::NonZeroU32, ptr::NonNull};
use std::{hash::Hash, sync::Mutex};

/// Multiplier for the offset in Handle
const INDEX_SCALE: usize = 2;

pub struct ArenaPool {
    base: *mut u8,
    used_committed: Mutex<(usize, usize)>,
}
impl ArenaPool {
    // just needs to be nonzero
    const INITIAL_OFFSET: usize = INDEX_SCALE;
    const RESERVE_SIZE: usize = INDEX_SCALE * (1 << 32);
    const COMMIT_SIZE: usize = 1 << 28; // 256MB
    const DEFAULT_SLICE_LEN: usize = 1 << 23; //  8MB

    pub fn new() -> Self {
        #[cfg(unix)]
        let ptr = {
            let ptr = unsafe {
                libc::mmap(
                    core::ptr::null_mut(),
                    Self::RESERVE_SIZE,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                // TODO: handle fallible allocations
                let err = std::io::Error::last_os_error();
                panic!("mmap failed: {}", err);
            }
            ptr
        };
        #[cfg(target_os = "windows")]
        let ptr = {
            use windows_sys::Win32::System::Memory;
            let ptr = unsafe {
                Memory::VirtualAlloc(
                    core::ptr::null(),
                    Self::RESERVE_SIZE,
                    Memory::MEM_RESERVE,
                    Memory::PAGE_NOACCESS,
                )
            };
            if ptr == core::ptr::null_mut() {
                // TODO: handle fallible allocations
                let err = std::io::Error::last_os_error();
                panic!("VirtualAlloc failed: {}", err);
            }
            ptr
        };
        #[cfg(not(any(unix, target_os = "windows")))]
        compile_error!("unimplemented");

        Self {
            base: ptr.cast::<u8>(),
            used_committed: Mutex::new((Self::INITIAL_OFFSET, 0)),
        }
    }

    pub fn sub_arena<'a>(&'a self) -> Arena<'a> {
        Arena {
            parent: self,
            base: self.base,
            offset: Self::INITIAL_OFFSET,
            rem_len: 0,
        }
    }

    fn alloc_slice(&self, min_size: usize) -> (usize, usize) {
        let size = min_size
            .max(Self::DEFAULT_SLICE_LEN)
            .next_multiple_of(INDEX_SCALE);

        let offset: usize;
        let mut guard = self.used_committed.lock().unwrap();
        let (used, committed) = *guard;
        offset = used;
        if used + size > committed {
            let start_ptr = unsafe { self.base.add(committed) };
            let commit_len = size.next_multiple_of(Self::COMMIT_SIZE);

            if committed + commit_len > Self::RESERVE_SIZE {
                // TODO: handle fallible allocations
                panic!("Exceeded Arena capacity");
            }

            #[cfg(unix)]
            {
                let ret = unsafe {
                    libc::mprotect(
                        start_ptr.cast(),
                        commit_len,
                        libc::PROT_READ | libc::PROT_WRITE,
                    )
                };
                if ret != 0 {
                    // TODO: handle fallible allocations
                    let err = std::io::Error::last_os_error();
                    panic!("mmap failed: {}", err);
                }
            }
            #[cfg(target_os = "windows")]
            {
                use windows_sys::Win32::System::Memory;
                let ret = unsafe {
                    Memory::VirtualAlloc(
                        start_ptr.cast(),
                        commit_len,
                        Memory::MEM_COMMIT,
                        Memory::PAGE_READWRITE,
                    )
                };
                if ret == core::ptr::null_mut() {
                    // TODO: handle fallible allocations
                    let err = std::io::Error::last_os_error();
                    panic!("VirtualAlloc failed: {}", err);
                }
            }
            #[cfg(not(any(unix, target_os = "windows")))]
            compile_error!("unimplemented");

            guard.1 += commit_len;
        }
        guard.0 += size;
        drop(guard);

        (offset, size)
    }

    /// Total number of bytes currently retained by this Arena.
    pub fn retained_size(&self) -> usize {
        self.used_committed.lock().unwrap().1
    }

    /// Bulk free. Retains committed memory (physical pages).
    pub fn reset(&mut self) {
        self.used_committed.get_mut().unwrap().0 = Self::INITIAL_OFFSET;
    }

    /// Bulk free. Releases committed memory (physical pages), retains virtual memory.
    pub fn reset_and_free(&mut self) {
        // exclusive borrow means no allocations into the arena can still be alive
        let used_committed = self.used_committed.get_mut().unwrap();
        #[cfg(unix)]
        {
            let ret = unsafe {
                libc::mmap(
                    self.base.cast(),
                    Self::RESERVE_SIZE,
                    libc::PROT_NONE,
                    libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                    -1,
                    0,
                )
            };
            if ret == libc::MAP_FAILED {
                // TODO: handle fallible allocations
                let err = std::io::Error::last_os_error();
                panic!("mmap failed: {}", err);
            }
        }
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::System::Memory;
            let committed = used_committed.1;
            let ret =
                unsafe { Memory::VirtualFree(self.base.cast(), committed, Memory::MEM_DECOMMIT) };
            if ret == 0 {
                // TODO: handle fallible allocations
                let err = std::io::Error::last_os_error();
                panic!("VirtualAlloc failed: {}", err);
            }
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        compile_error!("unimplemented");
        used_committed.0 = 4;
        used_committed.1 = 0;
    }
}

// `base` field is readonly
unsafe impl Sync for ArenaPool {}

impl Drop for ArenaPool {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let ret = unsafe { libc::munmap(self.base.cast(), Self::RESERVE_SIZE) };
            if ret == -1 {
                // TODO: handle fallible allocations
                let err = std::io::Error::last_os_error();
                panic!("munmap failed: {}", err);
            }
        }
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::System::Memory;
            let ret = unsafe {
                Memory::VirtualFree(
                    self.base.cast(),
                    Self::RESERVE_SIZE,
                    Memory::MEM_RELEASE, // decommits if necessary
                )
            };
            if ret == 0 {
                // TODO: handle fallible allocations
                let err = std::io::Error::last_os_error();
                panic!("VirtualAlloc failed: {}", err);
            }
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        compile_error!("unimplemented");
    }
}

pub struct Arena<'arena> {
    parent: &'arena ArenaPool,
    // duplicated to reduce indirection
    // pub super because NodeOptions needs access
    base: *mut u8,
    offset: usize,
    rem_len: usize,
}
impl<'arena> Arena<'arena> {
    /// Insert a single element, returns the handle
    pub fn alloc<T>(&mut self, e: T) -> Handle<'arena, T> {
        // valid len and align
        unsafe {
            let (o, p) = self.alloc_raw(align_of::<T>(), size_of::<T>());
            p.write(e);
            Handle(o, PhantomData)
        }
    }

    /// Insert a contiguous sequence of elements, returns the handle
    ///
    /// Safety: `elems` must accurately report its length (true for standard library types)
    pub unsafe fn alloc_slice<T>(
        &mut self,
        elems: impl ExactSizeIterator<Item = T>,
    ) -> SliceHandle<'arena, T> {
        let len = elems.len();
        unsafe {
            let (o, p) = self.alloc_raw(align_of::<T>(), len * size_of::<T>());
            for (idx, e) in elems.enumerate() {
                p.add(idx).write(e);
            }
            SliceHandle(o, len as u32, PhantomData)
        }
    }

    pub(super) unsafe fn alloc_node_options<T>(
        &mut self,
        align: usize,
        len: usize,
        head: super::move_options::Header,
        init: impl FnOnce(*mut u8),
    ) -> NodeOptionsHandle<'arena, T> {
        let (offset, ptr) = self.alloc_raw::<super::move_options::Header>(align, len);
        ptr.write(head);
        init(ptr.as_ptr().cast::<u8>());
        NodeOptionsHandle(offset, PhantomData)
    }

    /// Safety:
    /// - `align` must be a power of two
    /// - `len` must be a multiple of align
    ///
    /// The returned pointer has a lifetime of `'arena`.
    unsafe fn alloc_raw<T>(self: &mut Self, align: usize, len: usize) -> (NonZeroU32, NonNull<T>) {
        debug_assert!(align.is_power_of_two());
        debug_assert!(len.is_multiple_of(align));
        let mut align_offset = self.base.add(self.offset).align_offset(align);
        if self.rem_len < align_offset + len {
            // can't satisfy this request, have to get a new slice from the base allocator.
            self.refresh_backing_slice(align + len);
            align_offset = self.base.add(self.offset).align_offset(align);
        }
        // Safety: the request fits inside the backing slice.
        let given_offset = self.offset as usize + align_offset;
        let give = unsafe { self.base.byte_add(given_offset).cast() };
        let given_offset = NonZeroU32::new_unchecked((given_offset / INDEX_SCALE) as u32);
        self.offset += align_offset + len;
        self.rem_len -= align_offset + len;
        (given_offset, unsafe { NonNull::new_unchecked(give) })
    }

    #[cold]
    fn refresh_backing_slice(&mut self, min_size: usize) {
        let (offset, len) = self.parent.alloc_slice(min_size);
        self.offset = offset;
        self.rem_len = len;
    }
}

/// 4 byte offset into an Arena.
/// The offset is between 4 and u32::MAX.
pub struct Handle<'arena, T>(NonZeroU32, PhantomData<&'arena T>);

impl<'arena, T> Handle<'arena, T> {
    /// Safety: this `Handle` must have been derived from `arena`.
    pub fn resolve(&self, arena: &Arena<'arena>) -> &'arena T {
        unsafe {
            &*arena
                .base
                .add(self.0.get() as usize * INDEX_SCALE)
                .cast::<T>()
        }
    }
}
impl<'arena, T> Clone for Handle<'arena, T> {
    fn clone(&self) -> Self {
        Self(self.0, self.1)
    }
}
impl<'arena, T> Copy for Handle<'arena, T> {}
impl<'a, T> PartialEq for Handle<'a, T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<'a, T> Eq for Handle<'a, T> {}
impl<'a, T> Hash for Handle<'a, T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// 4 byte offset into an Arena.
/// The offset is between 4 and u32::MAX.
pub struct SliceHandle<'arena, T>(NonZeroU32, u32, PhantomData<&'arena T>);
impl<'arena, T> SliceHandle<'arena, T> {
    const _SCALE_ASSERT: () = assert!(size_of::<T>().is_multiple_of(INDEX_SCALE));
    /// Safety: this `Handle` must have been derived from `arena`.
    pub fn resolve(&self, arena: &Arena<'arena>) -> &'arena [T] {
        unsafe {
            core::slice::from_raw_parts(
                arena
                    .base
                    .add(self.0.get() as usize * INDEX_SCALE)
                    .cast::<T>(),
                self.1 as usize,
            )
        }
    }
    pub fn len(&self) -> usize {
        self.1 as usize
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Handle<'arena, T>> {
        const { assert!(size_of::<T>().is_multiple_of(INDEX_SCALE)) };

        let base = self.0.get();
        (0..self.len()).map(move |i| {
            Handle(
                unsafe {
                    NonZeroU32::new_unchecked(base + (i * size_of::<T>() / INDEX_SCALE) as u32)
                },
                PhantomData,
            )
        })
    }
}
impl<'arena, T> Clone for SliceHandle<'arena, T> {
    fn clone(&self) -> Self {
        Self(self.0, self.1, self.2)
    }
}
impl<'arena, T> Copy for SliceHandle<'arena, T> {}

pub struct NodeOptionsHandle<'arena, T>(
    pub(super) NonZeroU32,
    pub(super) PhantomData<&'arena super::NodeOptions<'arena, T>>,
);
impl<'arena, T> NodeOptionsHandle<'arena, T> {
    pub fn resolve(&self, arena: &Arena<'arena>) -> super::move_options::NodeOptions<'arena, T> {
        super::NodeOptions {
            ptr: unsafe {
                NonNull::new_unchecked(arena.base.add(self.0.get() as usize * INDEX_SCALE))
                    .cast::<super::move_options::Header>()
            },
            _phant: PhantomData,
        }
    }
}

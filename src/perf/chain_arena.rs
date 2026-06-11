#![allow(dead_code)]
use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::hash::Hash;

pub struct ArenaPool {
    inner: std::sync::Mutex<Inner>,
}
impl ArenaPool {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Inner::new()),
        }
    }

    pub fn sub_arena<'a>(&'a self) -> Arena<'a> {
        Arena {
            base: self,
            ptr: core::ptr::dangling_mut(),
            rem_len: 0,
        }
    }
    fn alloc_slice(&self, min_size: usize) -> *mut [u8] {
        self.inner.lock().unwrap().alloc_slice(min_size)
    }

    /// Total number of bytes currently retained by this Arena.
    pub fn retained_size(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .backing
            .iter()
            .map(|s| s.len())
            .sum()
    }

    pub fn reset(&mut self) {
        // exclusive borrow means no allocations into the arena can still be alive
        self.inner.get_mut().unwrap().reset()
    }
}
/// List of large allocated slices
struct Inner {
    backing: Vec<*mut [u8]>,
    // index into self.allocs, the slice currently being partitioned.
    current_idx: usize,
    // how much of the current slice has been consumed
    current_used_len: usize,
}
unsafe impl Send for Inner {}
impl Inner {
    const DEFAULT_CHUNK_LEN: usize = 1 << 26; // 64MB
    const DEFAULT_SLICE_LEN: usize = 1 << 23; //  8MB
    fn new() -> Self {
        let p = Self::alloc_chunk(Self::DEFAULT_CHUNK_LEN);
        Self {
            backing: vec![p],
            current_idx: 0,
            current_used_len: 0,
        }
    }
    fn alloc_chunk(len: usize) -> *mut [u8] {
        unsafe {
            let layout = Layout::from_size_align(len, 1).unwrap();
            let p = alloc(layout);
            if p.is_null() {
                handle_alloc_error(layout)
            }
            core::ptr::slice_from_raw_parts_mut(p, len)
        }
    }
    fn free_chunk(chunk: *mut [u8]) {
        unsafe {
            dealloc(
                chunk.cast::<u8>(),
                Layout::from_size_align_unchecked(chunk.len(), 1),
            );
        }
    }
    fn alloc_slice(&mut self, size: usize) -> *mut [u8] {
        // NOTE: this is more robust than it needs to be. Could just as well only deal in fixed
        // size slices

        let rem_len = self.backing[self.current_idx].len() - self.current_used_len;
        if rem_len < size {
            self.current_idx += 1;
            // Received extremely large request, free up reserved chunks that are too small.
            // unreachable in practice, actual allocs are tiny.
            while self.current_idx < self.backing.len()
                && self.backing[self.current_idx].len() < size
            {
                Self::free_chunk(self.backing.swap_remove(self.current_idx));
            }
            if self.current_idx == self.backing.len() {
                self.backing
                    .push(Self::alloc_chunk(size.max(Self::DEFAULT_CHUNK_LEN)));
            }
            self.current_used_len = 0;
        }
        let rem_len = self.backing[self.current_idx].len() - self.current_used_len;
        let given_size = size.max(Self::DEFAULT_SLICE_LEN.min(rem_len));
        let give = core::ptr::slice_from_raw_parts_mut(
            unsafe {
                self.backing[self.current_idx]
                    .cast::<u8>()
                    .add(self.current_used_len)
            },
            given_size,
        );
        self.current_used_len += given_size;
        give
    }
    fn reset(&mut self) {
        self.current_idx = 0;
        self.current_used_len = 0;
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        for chunk in self.backing.drain(..) {
            Self::free_chunk(chunk);
        }
    }
}

pub struct Arena<'arena> {
    base: &'arena ArenaPool,
    ptr: *mut u8,
    rem_len: usize,
}
impl<'arena> Arena<'arena> {
    pub fn alloc<T>(&mut self, e: T) -> Handle<'arena, T> {
        // valid len and align
        unsafe {
            let p = self.alloc_raw(align_of::<T>(), size_of::<T>());
            p.write(e);

            Handle(p.as_ref())
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
            let p = self.alloc_raw(align_of::<T>(), len * size_of::<T>());
            for (idx, e) in elems.enumerate() {
                p.add(idx).write(e);
            }
            SliceHandle(core::slice::from_raw_parts(p.as_ptr(), len))
        }
    }

    /// Safety: `init` must initialize the tail of the DST.
    pub(super) unsafe fn alloc_node_options<T>(
        &mut self,
        align: usize,
        len: usize,
        head: super::move_options::Header,
        init: impl FnOnce(*mut u8),
    ) -> NodeOptionsHandle<'arena, T> {
        let ptr = self.alloc_raw::<super::move_options::Header>(align, len);
        ptr.write(head);
        init(ptr.as_ptr().cast::<u8>());
        NodeOptionsHandle(super::NodeOptions {
            ptr,
            _phant: std::marker::PhantomData,
        })
    }

    /// Safety:
    /// - `align` must be a power of two
    /// - `len` must be a multiple of align
    ///
    /// The returned pointer has a lifetime of `'arena`.
    unsafe fn alloc_raw<T>(self: &mut Self, align: usize, len: usize) -> core::ptr::NonNull<T> {
        debug_assert!(align.is_power_of_two());
        debug_assert!(len.is_multiple_of(align));
        let mut align_offset = self.ptr.align_offset(align);
        if self.rem_len < align_offset + len {
            // can't satisfy this request, have to get a new slice from the base allocator.
            self.refresh_backing_slice(align + len);
            align_offset = self.ptr.align_offset(align);
        }
        // Safety: the request fits inside the backing slice.
        let give = unsafe { self.ptr.byte_add(align_offset).cast() };
        self.ptr = unsafe { self.ptr.byte_add(align_offset + len) };
        self.rem_len -= align_offset + len;
        unsafe { core::ptr::NonNull::new_unchecked(give) }
    }
    #[cold]
    fn refresh_backing_slice(&mut self, min_size: usize) {
        let new_slice = self.base.alloc_slice(min_size);
        self.ptr = new_slice.cast();
        self.rem_len = new_slice.len();
    }
}

pub struct Handle<'arena, T: Sized>(&'arena T);

impl<'arena, T> Handle<'arena, T> {
    pub fn resolve(&self, _arena: &Arena<'arena>) -> &'arena T {
        self.0
    }
}
impl<T> Clone for Handle<'_, T> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}
impl<T> Copy for Handle<'_, T> {}
impl<'a, T> PartialEq for Handle<'a, T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::addr_eq(self, other)
    }
}
impl<'a, T> Eq for Handle<'a, T> {}
impl<'a, T> Hash for Handle<'a, T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.0 as *const T).addr().hash(state);
    }
}

pub struct SliceHandle<'arena, T>(&'arena [T]);

impl<'arena, T> Clone for SliceHandle<'arena, T> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}
impl<'arena, T> Copy for SliceHandle<'arena, T> {}

impl<'arena, T> SliceHandle<'arena, T> {
    pub fn resolve(&self, _arena: &Arena<'arena>) -> &'arena [T] {
        self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Handle<'arena, T>> {
        self.0.iter().map(|r| Handle(r))
    }
}

pub struct NodeOptionsHandle<'arena, T>(super::move_options::NodeOptions<'arena, T>);

impl<'arena, T> NodeOptionsHandle<'arena, T> {
    pub fn resolve(&self, _arena: &Arena<'arena>) -> super::move_options::NodeOptions<'arena, T> {
        self.0
    }
}

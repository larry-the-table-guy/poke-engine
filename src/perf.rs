//! Low-level implementation details

/// Cumulative nanoseconds spent in each step of MCTS
#[derive(Default, Clone, Debug)]
#[repr(C)]
pub struct Timers {
    pub selection: u64,
    pub expand: u64,
    pub rollout: u64,
    pub backpropagate: u64,
    /// For multithreading; Time spent waiting on other threads.
    pub idle: u64,
}
impl Timers {
    pub fn add(&mut self, other: &Timers) {
        self.selection += other.selection;
        self.expand += other.expand;
        self.rollout += other.rollout;
        self.backpropagate += other.backpropagate;
    }
}

/// Bitset of [PokemonVolatileStatus]
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct VolatileStatusBitSet(u128);

use crate::engine::state::PokemonVolatileStatus;
impl VolatileStatusBitSet {
    pub const fn new() -> Self {
        Self(0)
    }
    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }
    pub const fn contains(&self, vs: &PokemonVolatileStatus) -> bool {
        self.0 & (1u128 << (*vs as u8)) != 0
    }
    pub const fn insert(&mut self, vs: PokemonVolatileStatus) {
        self.0 |= 1u128 << (vs as u8);
    }
    pub const fn remove(&mut self, vs: &PokemonVolatileStatus) -> bool {
        let present = self.contains(vs);
        self.0 &= !(1u128 << (*vs as u8));
        present
    }
    pub fn retain(&mut self, mut f: impl FnMut(PokemonVolatileStatus) -> bool) {
        let mut remaining = self.0;
        while remaining != 0 {
            let bit_index = remaining.trailing_zeros() as u8;
            let vs = PokemonVolatileStatus::from(bit_index);
            if !f(vs) {
                self.remove(&vs);
            }
            remaining &= remaining - 1
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = PokemonVolatileStatus> {
        let mut remaining = self.0;
        // you can be more efficient implementing it by hand, but iter isn't used anywhere
        // important
        std::iter::from_fn(move || {
            if remaining != 0 {
                let bit_index = remaining.trailing_zeros() as u8;
                remaining &= remaining - 1;
                Some(PokemonVolatileStatus::from(bit_index))
            } else {
                None
            }
        })
    }
    pub fn from_iter(iter: impl IntoIterator<Item = PokemonVolatileStatus>) -> Self {
        let mut s = Self::new();
        for vs in iter {
            s.insert(vs);
        }
        s
    }
    pub fn clear(&mut self) {
        self.0 = 0
    }
}

// A thin boxed slice with two sections.
//
// Follows the design of other thin smart pointers pretty closely.
pub use move_options::NodeOptions;
mod move_options {
    use crate::engine::state::MoveChoice;
    use crate::perf::arena::Arena;
    use std::convert::TryInto;
    #[repr(C)]
    pub struct NodeOptions<'a, T> {
        ptr: core::ptr::NonNull<Header>,
        _phant: core::marker::PhantomData<(T, &'a ())>,
    }
    #[repr(C)]
    struct Header {
        len_s1: u8,
        len_s2: u8,
    }
    impl<'a, T> NodeOptions<'a, T> {
        const DATA_OFFSET: usize = size_of::<Header>().next_multiple_of(align_of::<T>());

        pub fn new_in(
            arena: &mut Arena<'a>,
            s1: &[MoveChoice],
            s2: &[MoveChoice],
            mut ctor: impl FnMut(MoveChoice) -> T,
        ) -> Self {
            let total_len = s1.len() + s2.len();
            let len_s1: u8 = s1.len().try_into().unwrap();
            let len_s2: u8 = s2.len().try_into().unwrap();
            // Total len can't be more than 510, so we can't overflow usize
            unsafe {
                let ptr = arena.alloc_raw(
                    align_of::<Header>().max(align_of::<T>()),
                    Self::DATA_OFFSET + size_of::<T>() * total_len as usize,
                );

                ptr.write(Header { len_s1, len_s2 });
                let mut data_ptr = ptr.cast::<T>().byte_add(Self::DATA_OFFSET);
                for s in [s1, s2] {
                    for m in s {
                        data_ptr.write(ctor(*m));
                        data_ptr = data_ptr.add(1);
                    }
                }
                Self {
                    ptr,
                    _phant: core::marker::PhantomData,
                }
            }
        }

        /// The largest possible instance should not overflow an isize
        const _SIZE_ASSERT: () = assert!(
            size_of::<T>()
                .checked_mul(u8::MAX as usize * 2)
                .unwrap()
                .checked_add(Self::DATA_OFFSET)
                .unwrap()
                < isize::MAX as usize
        );
        fn header(&self) -> Header {
            unsafe { self.ptr.read() }
        }
        fn data_raw(&self) -> *mut T {
            unsafe { self.ptr.byte_add(Self::DATA_OFFSET).cast().as_ptr() }
        }
        fn parts(&self) -> (Header, &[T]) {
            let header = self.header();
            let len = header.len_s1 as usize + header.len_s2 as usize;
            (header, unsafe {
                std::slice::from_raw_parts(self.data_raw(), len)
            })
        }
        fn parts_mut(&mut self) -> (Header, &mut [T]) {
            let header = self.header();
            let len = header.len_s1 as usize + header.len_s2 as usize;
            (header, unsafe {
                std::slice::from_raw_parts_mut(self.data_raw(), len)
            })
        }

        // NOTE: these can be unchecked because only the constructor sets the lens
        pub fn s1(&self) -> &[T] {
            let (h, s) = self.parts();
            unsafe { s.get_unchecked(..h.len_s1 as usize) }
        }
        pub fn s1_mut(&mut self) -> &mut [T] {
            let (h, s) = self.parts_mut();
            unsafe { s.get_unchecked_mut(..h.len_s1 as usize) }
        }
        pub fn s2(&self) -> &[T] {
            let (h, s) = self.parts();
            unsafe { s.get_unchecked(h.len_s1 as usize..) }
        }
        pub fn s2_mut(&mut self) -> &mut [T] {
            let (h, s) = self.parts_mut();
            unsafe { s.get_unchecked_mut(h.len_s1 as usize..) }
        }
    }

    impl<'a, T> std::fmt::Debug for NodeOptions<'a, T>
    where
        T: std::fmt::Debug,
    {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MoveOptions")
                .field("s1", &self.s1())
                .field("s2", &self.s2())
                .finish()
        }
    }

    unsafe impl<'a, T> Sync for NodeOptions<'a, T>
    where
        Header: Sync,
        T: Sync,
    {
    }
    unsafe impl<'a, T> Send for NodeOptions<'a, T>
    where
        Header: Send,
        T: Send,
    {
    }
}

/// Bump allocator with fast mass free().
///
/// There are two parts:
/// - [ConcurrentArena], which owns the memory. It can spawn [Arena]s and perform a bulk reset.
/// - [Arena], a single-thread bump allocator which periodically fetches more memory from its
///   parent ConcurrentArena
///
/// Unlike `Bumpalo`, `Arena` requires `&mut` to allocate.
///
/// Destructors are not ran for objects inserted into the arena - when designing your structs,
/// you'll want to have a graph of objects that only point to other objects in the same arena.
/// If you insert objects containing a Box or Vec or String or etc, they will be leaked when
/// the arena gets reset or dropped. That is memory-safe behavior, but rarely desirable.
pub mod arena {
    use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};

    pub struct ConcurrentArena {
        inner: std::sync::Mutex<Inner>,
    }
    impl ConcurrentArena {
        pub fn new() -> Self {
            Self {
                inner: std::sync::Mutex::new(Inner::new()),
            }
        }

        pub fn handle<'a>(&'a self) -> Arena<'a> {
            Arena {
                base: self,
                ptr: core::ptr::null_mut(),
                rem_len: 0,
            }
        }
        fn alloc_slice(&self, min_size: usize) -> *mut [u8] {
            self.inner.lock().unwrap().alloc_slice(min_size)
        }

        /// Total number of bytes currently retained by this Arena.
        pub fn total(&self) -> usize {
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
    /// Linked list of large allocated slices
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
        base: &'arena ConcurrentArena,
        ptr: *mut u8,
        rem_len: usize,
    }
    impl<'arena> Arena<'arena> {
        pub fn alloc<T>(&mut self, e: T) -> &'arena mut T {
            // valid len and align
            unsafe {
                let p = self.alloc_raw(align_of::<T>(), size_of::<T>());
                p.write(e);
                &mut *p.as_ptr()
            }
        }
        pub fn alloc_slice<T>(
            &mut self,
            elems: impl ExactSizeIterator<Item = T>,
        ) -> &'arena mut [T] {
            let len = elems.len();
            unsafe {
                let p = self.alloc_raw(align_of::<T>(), len * size_of::<T>());
                for (idx, e) in elems.enumerate() {
                    p.add(idx).write(e);
                }
                core::slice::from_raw_parts_mut(p.as_ptr(), len)
            }
        }
        /// Safety:
        /// - `align` must be a power of two
        /// - `len` must be a multiple of align
        ///
        /// The returned pointer has a lifetime of `'arena`.
        pub unsafe fn alloc_raw<T>(
            self: &mut Self,
            align: usize,
            len: usize,
        ) -> core::ptr::NonNull<T> {
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
}

#[cfg(test)]
mod tests {
    // Run with 'cargo miri test' for more useful assertions about provenance and such
    #[test]
    fn node_options() {
        use super::arena;
        use crate::engine::state::MoveChoice;
        let ar = arena::ConcurrentArena::new();
        let ar = &mut ar.handle();
        type NodeOptions<'a> = super::NodeOptions<'a, MoveNode>;
        #[derive(Debug, PartialEq)]
        struct MoveNode(MoveChoice, f32, u32);
        // we don't expect to instantiate this with empty lists, but the logic should handle it.
        let s1 = &[];
        let s2 = &[];
        let a = NodeOptions::new_in(ar, s1, s2, |mc| MoveNode(mc, 0., 0));
        assert_eq!(a.s1().len(), 0);
        assert_eq!(a.s2().len(), 0);

        let s1 = &[MoveChoice::None, MoveChoice::None];
        let s2 = &[MoveChoice::None];
        let mut a = NodeOptions::new_in(ar, s1, s2, |mc| MoveNode(mc, 2., 3));
        assert_eq!(&a.s1().iter().map(|n| n.0).collect::<Vec<_>>(), s1);
        assert_eq!(&a.s2().iter().map(|n| n.0).collect::<Vec<_>>(), s2);
        a.s2_mut()[0].2 += 2;
        assert_eq!(&a.s1().iter().map(|n| n.0).collect::<Vec<_>>(), s1);
        assert_eq!(&a.s2().iter().map(|n| n.0).collect::<Vec<_>>(), s2);

        let s1 = &[MoveChoice::None, MoveChoice::None];
        let s2 = &[
            MoveChoice::None,
            MoveChoice::None,
            MoveChoice::None,
            MoveChoice::None,
        ];
        let mut a = NodeOptions::new_in(ar, s1, s2, |mc| MoveNode(mc, 2., 3));
        assert_eq!(&a.s1().iter().map(|n| n.0).collect::<Vec<_>>(), s1);
        assert_eq!(&a.s2().iter().map(|n| n.0).collect::<Vec<_>>(), s2);
        a.s1_mut()[1].2 += 2;
        assert_eq!(&a.s1().iter().map(|n| n.0).collect::<Vec<_>>(), s1);
        assert_eq!(&a.s2().iter().map(|n| n.0).collect::<Vec<_>>(), s2);
    }
}

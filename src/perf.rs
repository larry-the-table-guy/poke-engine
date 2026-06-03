//! Low-level implementation details

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
pub use move_options::{NodeOptions, NodeOptionsHandle};
mod move_options {
    use crate::engine::state::MoveChoice;
    use crate::perf::arena::Arena;
    use std::convert::TryInto;
    use std::marker::PhantomData;
    use std::num::NonZeroU32;
    #[repr(C)]
    pub struct NodeOptions<'a, T> {
        pub(super) ptr: core::ptr::NonNull<Header>,
        pub(super) _phant: core::marker::PhantomData<(T, &'a ())>,
    }
    #[repr(C)]
    pub(super) struct Header {
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
        ) -> NodeOptionsHandle<'a, T> {
            let total_len = s1.len() + s2.len();
            let len_s1: u8 = s1.len().try_into().unwrap();
            let len_s2: u8 = s2.len().try_into().unwrap();
            // Total len can't be more than 510, so we can't overflow usize
            unsafe {
                let (offset, ptr) = arena.alloc_raw(
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
                NodeOptionsHandle(offset, core::marker::PhantomData)
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

    pub struct NodeOptionsHandle<'arena, T>(NonZeroU32, PhantomData<(T, &'arena ())>);
    impl<'arena, T> NodeOptionsHandle<'arena, T> {
        pub fn resolve(&self, arena: &Arena<'arena>) -> NodeOptions<'arena, T> {
            super::NodeOptions {
                ptr: unsafe {
                    core::ptr::NonNull::new_unchecked(arena.base.add(self.0.get() as usize))
                        .cast::<Header>()
                },
                _phant: PhantomData,
            }
        }
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
    // TODO: branded lifetimes to make unchecked resolve sound
    use std::{
        alloc::{alloc, dealloc, handle_alloc_error, Layout},
        hash::Hash,
        marker::PhantomData,
        num::NonZeroU32,
        sync::atomic,
    };

    pub struct ConcurrentArena {
        base: *mut u8,
        used: atomic::AtomicUsize,
    }
    impl ConcurrentArena {
        const CAPACITY: usize = 1 << 32; // 4GB
        const DEFAULT_SLICE_LEN: usize = 1 << 23; //  8MB
        const LAYOUT: Layout = unsafe { Layout::from_size_align_unchecked(Self::CAPACITY, 4) };
        pub fn new() -> Self {
            let p = unsafe { alloc(Self::LAYOUT) };
            if p.is_null() {
                handle_alloc_error(Self::LAYOUT)
            }
            // Reserve the first 4 bytes so that Handles are all NonZero
            unsafe { p.cast::<[u8; 4]>().write([0; 4]) }
            Self {
                base: p,
                used: atomic::AtomicUsize::new(4),
            }
        }

        pub fn sub_arena<'a>(&'a self) -> Arena<'a> {
            Arena {
                parent: self,
                base: self.base,
                offset: 4,
                rem_len: 0,
            }
        }

        fn alloc_slice(&self, min_size: usize) -> (usize, usize) {
            let size = min_size.max(Self::DEFAULT_SLICE_LEN);
            let offset = self.used.fetch_add(size, atomic::Ordering::SeqCst);
            if offset + size > Self::CAPACITY {
                handle_alloc_error(unsafe { Layout::from_size_align_unchecked(size, 1) });
            }
            (offset, size)
        }

        pub fn reset(&mut self) {
            // exclusive borrow means no allocations into the arena can still be alive
            *self.used.get_mut() = 4;
        }
    }
    unsafe impl Sync for ConcurrentArena {}
    impl Drop for ConcurrentArena {
        fn drop(&mut self) {
            unsafe { dealloc(self.base, Self::LAYOUT) };
        }
    }

    /// 4 byte offset into an Arena.
    /// The offset is between 4 and u32::MAX.
    #[repr(transparent)]
    pub struct Handle<'arena, T>(NonZeroU32, PhantomData<&'arena T>);
    impl<'arena, T> Handle<'arena, T> {
        /// Safety: this `Handle` must have been derived from `arena`.
        pub fn resolve(&self, arena: &Arena<'arena>) -> &'arena T {
            unsafe { &*arena.base.add(self.0.get() as usize).cast::<T>() }
        }
    }
    impl<'arena, T> Clone for Handle<'arena, T> {
        fn clone(&self) -> Self {
            Self(self.0.clone(), self.1.clone())
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
        /// Safety: this `Handle` must have been derived from `arena`.
        pub fn resolve(&self, arena: &Arena<'arena>) -> &'arena [T] {
            unsafe {
                core::slice::from_raw_parts(
                    arena.base.add(self.0.get() as usize).cast::<T>(),
                    self.1 as usize,
                )
            }
        }
        pub fn len(&self) -> usize {
            self.1 as usize
        }
        pub fn iter(&self) -> impl ExactSizeIterator<Item = Handle<'arena, T>> {
            let base = self.0.get();
            (0..self.len()).map(move |i| {
                Handle(
                    unsafe { NonZeroU32::new_unchecked(base + (i * size_of::<T>()) as u32) },
                    PhantomData,
                )
            })
        }
    }
    impl<'arena, T> Clone for SliceHandle<'arena, T> {
        fn clone(&self) -> Self {
            Self(self.0.clone(), self.1.clone(), self.2.clone())
        }
    }
    impl<'arena, T> Copy for SliceHandle<'arena, T> {}

    pub struct Arena<'arena> {
        parent: &'arena ConcurrentArena,
        // duplicated to reduce indirection
        pub(super) base: *mut u8,
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
        /// Safety:
        /// - `align` must be a power of two
        /// - `len` must be a multiple of align
        ///
        /// The returned pointer has a lifetime of `'arena`.
        pub unsafe fn alloc_raw<T>(
            self: &mut Self,
            align: usize,
            len: usize,
        ) -> (NonZeroU32, core::ptr::NonNull<T>) {
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
            let given_offset = NonZeroU32::new_unchecked(given_offset as u32);
            self.offset += align_offset + len;
            self.rem_len -= align_offset + len;
            (given_offset, unsafe {
                core::ptr::NonNull::new_unchecked(give)
            })
        }
        #[cold]
        fn refresh_backing_slice(&mut self, min_size: usize) {
            let (offset, len) = self.parent.alloc_slice(min_size);
            self.offset = offset;
            self.rem_len = len;
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
        let ar = &mut ar.sub_arena();
        type NodeOptions<'a> = super::NodeOptions<'a, MoveNode>;
        #[derive(Debug, PartialEq)]
        struct MoveNode(MoveChoice, f32, u32);
        // we don't expect to instantiate this with empty lists, but the logic should handle it.
        let s1 = &[];
        let s2 = &[];
        let a = NodeOptions::new_in(ar, s1, s2, |mc| MoveNode(mc, 0., 0));
        let a = a.resolve(ar);
        assert_eq!(a.s1().len(), 0);
        assert_eq!(a.s2().len(), 0);

        let s1 = &[MoveChoice::None, MoveChoice::None];
        let s2 = &[MoveChoice::None];
        let a = NodeOptions::new_in(ar, s1, s2, |mc| MoveNode(mc, 2., 3));
        let mut a = a.resolve(ar);
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
        let a = NodeOptions::new_in(ar, s1, s2, |mc| MoveNode(mc, 2., 3));
        let mut a = a.resolve(ar);
        assert_eq!(&a.s1().iter().map(|n| n.0).collect::<Vec<_>>(), s1);
        assert_eq!(&a.s2().iter().map(|n| n.0).collect::<Vec<_>>(), s2);
        a.s1_mut()[1].2 += 2;
        assert_eq!(&a.s1().iter().map(|n| n.0).collect::<Vec<_>>(), s1);
        assert_eq!(&a.s2().iter().map(|n| n.0).collect::<Vec<_>>(), s2);
    }
}

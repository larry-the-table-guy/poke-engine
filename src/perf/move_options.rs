//! A thin boxed slice with two sections.
//!
//! Follows the design of other thin smart pointers pretty closely.

use crate::engine::state::MoveChoice;
use crate::perf::arena::Arena;
use std::convert::TryInto;
#[repr(C)]
pub struct NodeOptions<'a, T> {
    pub(super) ptr: core::ptr::NonNull<Header>,
    pub(super) _phant: core::marker::PhantomData<(T, &'a ())>,
}

impl<'a, T> Clone for NodeOptions<'a, T> {
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr,
            _phant: self._phant,
        }
    }
}
impl<'a, T> Copy for NodeOptions<'a, T> {}

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
    ) -> super::arena::NodeOptionsHandle<'a, T> {
        let total_len = s1.len() + s2.len();
        let len_s1: u8 = s1.len().try_into().unwrap();
        let len_s2: u8 = s2.len().try_into().unwrap();
        // Total len can't be more than 510, so we can't overflow usize
        unsafe {
            arena.alloc_node_options(
                align_of::<Header>().max(align_of::<T>()),
                Self::DATA_OFFSET + size_of::<T>() * total_len as usize,
                Header { len_s1, len_s2 },
                |ptr| {
                    let mut data_ptr = ptr.cast::<T>().byte_add(Self::DATA_OFFSET);
                    for s in [s1, s2] {
                        for m in s {
                            data_ptr.write(ctor(*m));
                            data_ptr = data_ptr.add(1);
                        }
                    }
                },
            )
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

    fn parts(&self) -> (Header, &[T]) {
        let header = unsafe { self.ptr.read() };
        let len = header.len_s1 as usize + header.len_s2 as usize;
        let data_ptr = unsafe { self.ptr.byte_add(Self::DATA_OFFSET).cast().as_ptr() };
        (header, unsafe { std::slice::from_raw_parts(data_ptr, len) })
    }

    // NOTE: these can be unchecked because only the constructor sets the lens
    pub fn s1(&self) -> &[T] {
        let (h, s) = self.parts();
        unsafe { s.get_unchecked(..h.len_s1 as usize) }
    }
    pub fn s2(&self) -> &[T] {
        let (h, s) = self.parts();
        unsafe { s.get_unchecked(h.len_s1 as usize..) }
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

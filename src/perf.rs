//! Low-level implementation details

// A thin boxed slice with two sections.
//
// Follows the design of other thin smart pointers pretty closely.
pub use move_options::NodeOptions;
mod move_options {
    use crate::engine::state::MoveChoice;
    use std::convert::TryInto;
    use std::{
        alloc::{alloc, handle_alloc_error, Layout},
        ptr::NonNull,
    };
    #[repr(C)]
    pub struct NodeOptions<T> {
        ptr: core::ptr::NonNull<Header>,
        _phant: core::marker::PhantomData<T>,
    }
    #[repr(C)]
    struct Header {
        len_s1: u8,
        len_s2: u8,
    }
    impl<T> NodeOptions<T> {
        const DATA_OFFSET: usize = size_of::<Header>().next_multiple_of(align_of::<T>());

        pub fn new(
            s1: &[MoveChoice],
            s2: &[MoveChoice],
            mut ctor: impl FnMut(MoveChoice) -> T,
        ) -> Self {
            let total_len = s1.len() + s2.len();
            let len_s1: u8 = s1.len().try_into().unwrap();
            let len_s2: u8 = s2.len().try_into().unwrap();
            // Total len can't be more than 510, so we can't overflow usize
            unsafe {
                let layout = Self::layout(total_len as u16);
                let Some(ptr) = NonNull::new(alloc(layout) as *mut Header) else {
                    handle_alloc_error(layout);
                };
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

        fn layout(total_len: u16) -> Layout {
            // SAFETY:
            // - align is not zero
            // - align is power of two
            // - size cannot overflow isize
            unsafe {
                Layout::from_size_align_unchecked(
                    Self::DATA_OFFSET + size_of::<T>() * total_len as usize,
                    align_of::<Header>().max(align_of::<T>()),
                )
            }
        }
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
    impl<T> Drop for NodeOptions<T> {
        fn drop(&mut self) {
            let slice = self.parts_mut().1;
            let total_len = slice.len() as u16;
            unsafe { core::ptr::drop_in_place(slice) }
            unsafe { std::alloc::dealloc(self.ptr.as_ptr() as *mut u8, Self::layout(total_len)) }
        }
    }
    impl<T> std::fmt::Debug for NodeOptions<T>
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

    unsafe impl<T> Sync for NodeOptions<T>
    where
        Header: Sync,
        T: Sync,
    {
    }
    unsafe impl<T> Send for NodeOptions<T>
    where
        Header: Send,
        T: Send,
    {
    }
}

#[cfg(test)]
mod tests {
    // Run with 'cargo miri test' for more useful assertions about provenance and such
    #[test]
    fn node_options() {
        use crate::engine::state::MoveChoice;
        type NodeOptions = super::NodeOptions<MoveNode>;
        #[derive(Debug, PartialEq)]
        struct MoveNode(MoveChoice, f32, u32);
        // we don't expect to instantiate this with empty lists, but the logic should handle it.
        let s1 = &[];
        let s2 = &[];
        let a = NodeOptions::new(s1, s2, |mc| MoveNode(mc, 0., 0));
        assert_eq!(a.s1().len(), 0);
        assert_eq!(a.s2().len(), 0);

        let s1 = &[MoveChoice::None, MoveChoice::None];
        let s2 = &[MoveChoice::None];
        let mut a = NodeOptions::new(s1, s2, |mc| MoveNode(mc, 2., 3));
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
        let mut a = NodeOptions::new(s1, s2, |mc| MoveNode(mc, 2., 3));
        assert_eq!(&a.s1().iter().map(|n| n.0).collect::<Vec<_>>(), s1);
        assert_eq!(&a.s2().iter().map(|n| n.0).collect::<Vec<_>>(), s2);
        a.s1_mut()[1].2 += 2;
        assert_eq!(&a.s1().iter().map(|n| n.0).collect::<Vec<_>>(), s1);
        assert_eq!(&a.s2().iter().map(|n| n.0).collect::<Vec<_>>(), s2);
    }
}

//! Low-level implementation details

pub use arena::NodeOptionsHandle;
pub use bit_set::VolatileStatusBitSet;
pub use move_options::NodeOptions;

mod bit_set;
mod move_options;

#[cfg(all(target_pointer_width = "64", any(unix, target_os = "windows")))]
mod virt_arena;

mod chain_arena;

/// Bump allocator with fast bulk free().
///
/// There are three parts:
/// - [ArenaPool], which owns the memory, spawns `Arena`s and perform a bulk reset.
/// - [Arena], a single-thread bump allocator which produces `Handle`s
///   and periodically fetches more memory from its parent `ArenaPool`
/// - [Handle]
///
/// Unlike `Bumpalo`, `Arena` requires `&mut` to allocate.
///
/// Destructors are not ran for objects inserted into the arena - when designing your structs,
/// you'll want to have a graph of objects that only point to other objects in the same arena.
/// If you insert objects containing a Box or Vec or String or etc, they will be leaked when
/// the arena gets reset or dropped. That is memory-safe behavior, but rarely desirable.
pub mod arena {
    cfg_select! {
        all(not(miri), target_pointer_width = "64", any(unix, target_os = "windows")) => {
            pub use super::virt_arena::*;
        },
        _ => { pub use super::chain_arena::*; }
    }
}

#[cfg(test)]
mod tests {

    // Run with 'cargo miri test' for more useful assertions about provenance and such
    #[test]
    fn node_options() {
        use super::arena;
        use crate::engine::state::MoveChoice;
        let ar = arena::ArenaPool::new();
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
        let a = a.resolve(ar);
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
        let a = a.resolve(ar);
        assert_eq!(&a.s1().iter().map(|n| n.0).collect::<Vec<_>>(), s1);
        assert_eq!(&a.s2().iter().map(|n| n.0).collect::<Vec<_>>(), s2);
    }
}

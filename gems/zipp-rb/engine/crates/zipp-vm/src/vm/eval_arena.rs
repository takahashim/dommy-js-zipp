//! Storage for the functions and classes a state installs at run time (an
//! `eval`, `new Function`, a script or module a host loads into it), owned by
//! the state itself so that dropping it frees them.
//!
//! They used to be `Box::leak`ed: the run loop and the JIT keep raw pointers
//! into a `FuncProto`, so it must never move, and a leaked box never does. But
//! a leak is never freed, so a host that creates and drops a state per page
//! grew by the compiled form of everything it loaded into each (about 1.2 MB a
//! page for dommy's bridge runtime). An [`Arena`] gives the same stable
//! addresses for as long as its owner lives, and frees them with it.
//!
//! Safe code only (`safe-sandbox` forbids `unsafe` in this crate): values sit
//! in `OnceCell<Box<T>>` slots of fixed-size chunks linked through further
//! `OnceCell`s, so `alloc(&self)` never moves anything it handed out before.

use std::cell::{Cell, OnceCell};

/// Slots per chunk. A slot is one pointer wide (the value is boxed), so an
/// unused chunk costs 2 KiB; reaching slot `n` walks `n / CHUNK` links.
const CHUNK: usize = 256;

struct Chunk<T> {
    items: Box<[OnceCell<Box<T>>]>,
    next: OnceCell<Box<Chunk<T>>>,
}

impl<T> Chunk<T> {
    fn new() -> Self {
        Chunk {
            items: (0..CHUNK).map(|_| OnceCell::new()).collect(),
            next: OnceCell::new(),
        }
    }
}

/// An append-only store whose values keep their addresses until it is dropped.
pub(crate) struct Arena<T> {
    head: Chunk<T>,
    len: Cell<usize>,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena {
            head: Chunk::new(),
            len: Cell::new(0),
        }
    }
}

impl<T> Arena<T> {
    /// Store `value` and return it, borrowed for as long as the arena lives.
    pub(crate) fn alloc(&self, value: T) -> &T {
        let i = self.len.get();
        self.len.set(i + 1);
        let mut chunk = &self.head;
        for _ in 0..i / CHUNK {
            chunk = chunk.next.get_or_init(|| Box::new(Chunk::new()));
        }
        let slot = &chunk.items[i % CHUNK];
        if slot.set(Box::new(value)).is_err() {
            unreachable!("eval arena slot {i} filled twice");
        }
        slot.get().expect("just filled")
    }
}

/// What a [`crate::vm::Vm`] installs at run time, owned beside its `Program`.
#[derive(Default)]
pub(crate) struct EvalArena {
    pub(crate) funcs: Arena<crate::bytecode::FuncProto>,
    pub(crate) classes: Arena<crate::bytecode::ClassDef>,
}

#[cfg(test)]
mod tests {
    use super::Arena;

    #[test]
    fn values_keep_their_addresses_across_chunks() {
        let arena = Arena::default();
        let first: *const String = arena.alloc(String::from("first"));
        let refs: Vec<&String> = (0..1000).map(|i| arena.alloc(i.to_string())).collect();
        assert_eq!(first, arena_first(&arena));
        assert_eq!(refs[999], "999");
        assert_eq!(refs[256], "256");
    }

    fn arena_first(arena: &Arena<String>) -> *const String {
        &**arena.head.items[0].get().unwrap()
    }
}

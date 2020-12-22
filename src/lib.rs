use std::alloc::{alloc, dealloc, handle_alloc_error, realloc, Layout};
use std::cell::Cell;
use std::mem::{self, MaybeUninit};
use std::ptr;

trait RefCnt {
    
}

#[derive(Default)]
#[repr(transparent)]
struct RCell(Cell<u16>);

/// The header of block of CoWec.
///
/// This is just the header part, usually followed by an dynamically sized array of `T`s.
///
/// The `len` field encodes both the current active length and the capacity. It can't be created or
/// destroyed directly, as it needs direct talking to a memory allocator.
///
/// repr(C) so we can control the layout. The align(2) is to make sure that we can abuse the last
/// bit of the pointer for a tag to denote an enum of one or other T.
#[repr(C, align(2))]
struct CoWecBlock<R, T> {
    /// Reference count, of some implementation.
    ///
    /// We can choose if we are thread safe or not by this (eg. equivalent to Rc vs Arc).
    rcell: R,
    /// The length & capacity.
    ///
    /// The upper 4 bits are the capacity. If the capacity is set to 0, it is „tight“ ‒ exactly the
    /// same number of slots as they are needed. Other value `i` denotes that there are `2^i`
    /// slots. The idea is that once we start sharing the block, it can't change any more and we
    /// can shrink it, but until then we use the classical doubling strategy.
    ///
    /// The rest 12 bits denote the used length.
    len: u16,

    /// The actual payload.
    ///
    /// We actually allocate enough, according to the capacity. The size of 0 is just a trick to
    /// make Rust do the right thing.
    data: [MaybeUninit<T>; 0],
}

impl<R: Default, T> CoWecBlock<R, T> {
    const LEN_MASK: u16 = 0b0000_1111_1111_1111;
    const CAP_OFFSET: u16 = 12;

    fn len(&self) -> usize {
        (self.len & Self::LEN_MASK) as usize
    }

    fn capacity(&self) -> usize {
        let cap = (self.len >> Self::CAP_OFFSET) as u32;
        if cap == 0 {
            self.len()
        } else {
            2usize.pow(cap)
        }
    }

    fn layout(capacity: usize) -> Layout {
        let head = Layout::new::<Self>();
        let tail = Layout::array::<MaybeUninit<T>>(capacity).expect("Invalid array layout");
        head.extend(tail).expect("Invalid layout created").0
    }

    /// A very much manual destructor. We can't really do proper Drop due to talking to the
    /// allocator.
    unsafe fn dispose(me: *mut Self) {
        let me_ref = me.as_mut().expect("Got invalid pointer to dispose");
        let layout = Self::layout(me_ref.capacity());
        ptr::drop_in_place(&mut me_ref.rcell);
        if mem::needs_drop::<T>() {
            let len = me_ref.len();
            let base = me_ref.data.as_mut_ptr();
            for i in 0..len {
                let elem: &mut MaybeUninit<_> = &mut *base.add(i);
                ptr::drop_in_place(elem.as_mut_ptr()); // Drop the thing *inside* the MaybeUninit
            }
        }
        dealloc(me.cast(), layout);
    }

    unsafe fn create(capacity: usize) -> *mut Self {
        assert!(capacity.is_power_of_two());
        // TODO: Range check?
        let cap_encoded = capacity.trailing_zeros() as u16;
        let layout = Self::layout(capacity);
        let header = Self {
            rcell: R::default(),
            len: cap_encoded << Self::CAP_OFFSET,
            data: [],
        };
        assert_eq!(header.capacity(), capacity);
        assert_eq!(header.len(), 0);
        let me = alloc(layout).cast::<Self>();
        if me.is_null() {
            handle_alloc_error(layout);
        }
        ptr::write(me, header);

        me
    }

    unsafe fn resize(me: *mut Self, new_cap: usize) -> *mut Self {
        assert!(new_cap.is_power_of_two());
        // TODO: Cap range check
        let cap_encoded = new_cap.trailing_zeros() as u16;
        let me_ref = me.as_mut().expect("Got invalid pointer to resize");
        let old_layout = Self::layout(me_ref.capacity());
        let new_layout = Self::layout(new_cap);
        let old_len = me_ref.len();
        let new_me = realloc(me.cast(), old_layout, new_layout.size()).cast::<Self>();
        if new_me.is_null() {
            handle_alloc_error(new_layout);
        }

        let me_ref = new_me.as_mut().unwrap();
        me_ref.len = (me_ref.len & Self::LEN_MASK) | (cap_encoded << Self::CAP_OFFSET);
        assert_eq!(me_ref.capacity(), new_cap);
        assert_eq!(me_ref.len(), old_len);
        new_me
    }

    
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test some allocation routines (create/resize/dispose).
    ///
    /// Aimed for valgrind and/or miri testing, mostly, to see if we are not doing ugly things in
    /// there.
    #[test]
    fn allocation() {
        type B = CoWecBlock::<RCell, String>;
        unsafe {
            let mut me = B::create(4);
            let mut me_ref = &*me;
            assert_eq!(me_ref.len(), 0);
            assert_eq!(me_ref.capacity(), 4);
            me = B::resize(me, 8);
            me_ref = &*me;
            assert_eq!(me_ref.len(), 0);
            assert_eq!(me_ref.capacity(), 8);
            B::dispose(me);
        }
    }
}

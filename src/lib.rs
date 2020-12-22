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
    const DATA_OFFSET: usize = Layout::new::<Self>().size();

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

    unsafe fn get_data_mut(me: *mut Self) -> *mut MaybeUninit<T> {
        me.cast::<u8>().add(Self::DATA_OFFSET).cast()
    }

    unsafe fn get_data(me: *const Self) -> *const MaybeUninit<T> {
        me.cast::<u8>().add(Self::DATA_OFFSET).cast()
    }

    /// A very much manual destructor. We can't really do proper Drop due to talking to the
    /// allocator.
    unsafe fn dispose(me: *mut Self) {
        let data = Self::get_data_mut(me);
        let me_ref = me.as_mut().expect("Got invalid pointer to dispose");
        let layout = Self::layout(me_ref.capacity());
        ptr::drop_in_place(&mut me_ref.rcell);
        if mem::needs_drop::<T>() {
            let len = me_ref.len();
            for i in 0..len {
                let elem: &mut MaybeUninit<_> = &mut *data.add(i);
                ptr::drop_in_place(elem.as_mut_ptr()); // Drop the thing *inside* the MaybeUninit
            }
        }
        dealloc(me.cast(), layout);
    }

    unsafe fn create(capacity: usize) -> *mut Self {
        debug_assert!(capacity.is_power_of_two());
        // TODO: Range check?
        let cap_encoded = capacity.trailing_zeros() as u16;
        let layout = Self::layout(capacity);
        let header = Self {
            rcell: R::default(),
            len: cap_encoded << Self::CAP_OFFSET,
            data: [],
        };
        debug_assert_eq!(header.capacity(), capacity);
        debug_assert_eq!(header.len(), 0);
        let me = alloc(layout).cast::<Self>();
        if me.is_null() {
            handle_alloc_error(layout);
        }
        ptr::write(me, header);

        me
    }

    unsafe fn resize(me: *mut Self, new_cap: usize) -> *mut Self {
        debug_assert!(new_cap.is_power_of_two());
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
        debug_assert_eq!(me_ref.capacity(), new_cap);
        debug_assert_eq!(me_ref.len(), old_len);
        new_me
    }

    unsafe fn insert(me: *mut Self, pos: usize, val: T) {
        let data = Self::get_data_mut(me);
        let me_ref = &mut *me;
        debug_assert!(me_ref.len() < me_ref.capacity(), "Over current capacity");
        debug_assert!(pos <= me_ref.len(), "Position out of range");
        let new_len = me_ref.len() as u16 + 1;
        debug_assert_eq!(new_len & Self::LEN_MASK, new_len, "Can't encode new length {:b}", new_len);
        let ptr_pos = data.add(pos);
        ptr::copy(ptr_pos, ptr_pos.add(1), me_ref.len() - pos);
        let elem = &mut *data.add(pos);
        ptr::write(elem.as_mut_ptr(), val);
        me_ref.len = (me_ref.len & !Self::LEN_MASK) | new_len;
    }

    unsafe fn remove(me: *mut Self, pos: usize) -> T {
        let data = Self::get_data_mut(me);
        let me_ref = &mut *me;
        debug_assert!(pos < me_ref.len());
        let ptr_pos = data.add(pos);
        let elem = ptr::read(ptr_pos).assume_init();
        ptr::copy(ptr_pos.add(1), ptr_pos, me_ref.len() - pos - 1);
        me_ref.len -= 1; // len must be >0 by now, so no underflow and touching the capacity
        elem
    }

    unsafe fn get<'a>(me: *const Self, pos: usize) -> &'a T {
        let data = Self::get_data(me);
        let me_ref = &*me;
        debug_assert!(pos < me_ref.len());
        let elem = data.add(pos);
        &*(*elem).as_ptr()
    }

    unsafe fn get_mut<'a>(me: *mut Self, pos: usize) -> &'a mut T {
        let data = Self::get_data_mut(me);
        let me_ref = &*me;
        debug_assert!(pos < me_ref.len());
        let elem = data.add(pos);
        &mut *(*elem).as_mut_ptr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type B = CoWecBlock::<RCell, String>;

    /// Test some allocation routines (create/resize/dispose).
    ///
    /// Aimed for valgrind and/or miri testing, mostly, to see if we are not doing ugly things in
    /// there.
    #[test]
    fn allocation() {
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

    #[test]
    fn insert_end() {
        unsafe {
            let me = B::create(4);
            B::insert(me, 0, "Hello".to_owned());
            let me_ref = &mut *me;
            assert_eq!(me_ref.len(), 1);
            assert_eq!(me_ref.capacity(), 4);
            assert_eq!(B::get(me, 0), "Hello");
            B::insert(me, 1, "World".to_owned());
            let me_ref = &mut *me;
            assert_eq!(me_ref.len(), 2);
            assert_eq!(me_ref.capacity(), 4);
            assert_eq!(B::get(me, 0), "Hello");
            assert_eq!(B::get(me, 1), "World");
            B::dispose(me);
        }
    }

    #[test]
    fn insert_beginning() {
        unsafe {
            let me = B::create(4);
            B::insert(me, 0, "Hello".to_owned());
            let me_ref = &mut *me;
            assert_eq!(me_ref.len(), 1);
            assert_eq!(me_ref.capacity(), 4);
            assert_eq!(B::get(me, 0), "Hello");
            B::insert(me, 0, "World".to_owned());
            let me_ref = &mut *me;
            assert_eq!(me_ref.len(), 2);
            assert_eq!(me_ref.capacity(), 4);
            assert_eq!(B::get(me, 0), "World");
            assert_eq!(B::get(me, 1), "Hello");
            B::dispose(me);
        }
    }

    #[test]
    fn replace() {
        unsafe {
            let me = B::create(4);
            B::insert(me, 0, "Hello".to_owned());
            *B::get_mut(me, 0) = "World".to_owned();
            let me_ref = &mut *me;
            assert_eq!(me_ref.len(), 1);
            assert_eq!(me_ref.capacity(), 4);
            assert_eq!(B::get(me, 0), "World");
            B::dispose(me);
        }
    }

    #[test]
    fn remove() {
        unsafe {
            let me = B::create(4);
            B::insert(me, 0, "Hello".to_owned());
            B::insert(me, 1, "World".to_owned());
            assert_eq!(B::remove(me, 0), "Hello");
            assert_eq!(B::remove(me, 0), "World");
            let me_ref = &mut *me;
            assert_eq!(me_ref.len(), 0);
            assert_eq!(me_ref.capacity(), 4);
            B::dispose(me);
        }
    }
}

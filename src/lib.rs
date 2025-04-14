#![allow(internal_features)]
#![cfg_attr(docsrs, feature(rustdoc_internals))]
#![doc = include_str!("../README.md")]
#![cfg_attr(doc, deny(missing_docs))]
#![no_std]
extern crate alloc;

use alloc::{
    alloc::{Layout, LayoutError, alloc, dealloc, handle_alloc_error},
    boxed::Box,
    vec::Vec,
};
use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::{ManuallyDrop, MaybeUninit},
    ops::{Deref, DerefMut},
    panic::{RefUnwindSafe, UnwindSafe},
    ptr::{NonNull, addr_eq, null_mut, slice_from_raw_parts, slice_from_raw_parts_mut},
    sync::atomic::{AtomicUsize, Ordering::*},
};

use crossbeam_utils::{Backoff, CachePadded};
use likely_stable::{LikelyResult, likely, unlikely};

/// Most significant bit used as a resource acquisition flag.
const FLAG: usize = 1 << (usize::BITS - 1);
/// Available bits used as index, fitting [`isize::MAX`].
const MASK: usize = !FLAG;

/// Dynamically-sized fragment that backs up the memory for [`VecBelt<T>`].
#[repr(C)]
struct Fragment<T> {
    /// Pointer to the next fragment. If this is [`null`](null_mut), then [`len`](Self::len) is `0`
    /// and therefore invalid.
    next: *mut Self,
    /// How many initialized elements are contained within this fragment.
    len: usize,
    /// The actual backing memory for elements. This is stored as a direct slice and not a pointer
    /// to improve cache locality.
    data: [MaybeUninit<T>],
}

impl<T> Fragment<T> {
    /// Returns the [`Layout`] for fragments of the given capacity.
    #[inline]
    fn layout(capacity: usize) -> Result<Layout, LayoutError> {
        // Extend the layout field-by-field...
        let (layout, ..) = Layout::new::<*mut Self>().extend(Layout::new::<usize>())?;
        let (layout, ..) = layout.extend(Layout::array::<MaybeUninit<T>>(capacity)?)?;

        // ...and pad it to its alignment, as a requirement for `repr(C)`.
        Ok(layout.pad_to_align())
    }

    /// Allocates a new fragment of the given capacity, returning a non-null pointer to it and its
    /// data. Initializes [`next`](Self::next) to [`null`](null_mut) and [`len`](Self::len) to `0`.
    #[inline]
    fn new(capacity: usize) -> Result<(NonNull<Self>, *mut T), LayoutError> {
        let layout = Self::layout(capacity)?;

        // Currently, allocation failures are treated as fatal. This might change in the future.
        let ptr = slice_from_raw_parts_mut(unsafe { alloc(layout) } as *mut (), capacity) as *mut Self;
        if ptr.is_null() {
            handle_alloc_error(layout)
        }

        // Initialize fields accordingly.
        unsafe {
            (&raw mut (*ptr).next).write(slice_from_raw_parts_mut(null_mut::<()>(), 0) as *mut Self);
            (&raw mut (*ptr).len).write(0)
        }

        Ok(unsafe { (NonNull::new_unchecked(ptr), &raw mut (*ptr).data as *mut T) })
    }
}

/// A high-performant, concurrent, lock-free data structure suitable for multi-threaded
/// bulk-appending (takes a `&self`) and single-threaded consuming (takes a `&mut self`).
///
/// See the [module-level](crate) documentation for more details.
pub struct VecBelt<T> {
    /// The total length of all fragments before the tail. This is used to offset the current total
    /// length to reside within the tail fragment.
    preceding_len: UnsafeCell<usize>,
    /// The synchronization primitive, contains the current total length and an [acquisition
    /// bit](FLAG).
    len: CachePadded<AtomicUsize>,
    /// The first fragment that contains elements starting from index 0.
    head: NonNull<Fragment<T>>,
    /// The tail fragment that threads are currently modifying.
    tail: UnsafeCell<NonNull<Fragment<T>>>,
}

/// # Safety
///
/// [`VecBelt<T>`] provides synchronization primitives that won't lead to data races.
unsafe impl<T: Send> Send for VecBelt<T> {}
/// # Safety
///
/// `T: Sync` isn't necessary here, because there is guaranteed not to be references to the same
/// elements across threads.
unsafe impl<T: Send> Sync for VecBelt<T> {}

impl<T> UnwindSafe for VecBelt<T> {}
impl<T> RefUnwindSafe for VecBelt<T> {}

impl<T> VecBelt<T> {
    /// Creates a new [`VecBelt<T>`], preallocating a fragment of the given initial size.
    #[inline]
    pub fn new(initial_size: usize) -> Self {
        let (head, ..) = Fragment::new(initial_size).expect("couldn't allocate a fragment");
        Self {
            preceding_len: UnsafeCell::new(0),
            len: CachePadded::new(AtomicUsize::new(0)),
            head,
            tail: UnsafeCell::new(head),
        }
    }

    /// Reads the length of the vector atomically. Prefer [`len_mut`](Self::len_mut) if possible.
    #[inline]
    pub fn len(&self) -> usize {
        self.len.load(Relaxed)
    }

    /// Tests whether the vector is empty atomically. Prefer [`is_empty_mut`](Self::is_empty_mut) if
    /// possible.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len.load(Relaxed) == 0
    }

    /// Reads the length of the vector non-atomically via `&mut self`.
    #[inline]
    pub fn len_mut(&mut self) -> usize {
        *self.len.get_mut()
    }

    /// Tests whether the vector is empty non-atomically via `&mut self`.
    #[inline]
    pub fn is_empty_mut(&mut self) -> bool {
        *self.len.get_mut() == 0
    }

    /// Extends the vector by `additional` elements, returning an uninitialized slice to it and the
    /// starting element's absolute index in the vector.
    ///
    /// # Safety
    /// `additional` must be less or equal to [`isize::MAX`].
    unsafe fn append_raw_erased(&self, additional: usize) -> (*mut T, usize) {
        let backoff = Backoff::new();
        let preceding_len = self.preceding_len.get();
        let tail = self.tail.get();

        return loop {
            unsafe {
                // Queries the current length of the vector, ignoring the flag.
                let len = self.len.load(Relaxed) & MASK;
                let new_len = len.unchecked_add(additional);

                if unlikely(new_len & FLAG == FLAG) {
                    panic!("too many elements")
                }

                // If the current length is exactly `len` without the flag, then store it as `new_len` with the
                // flag, momentarily locking the vector...
                if self.len.compare_exchange(len, new_len | FLAG, Acquire, Relaxed).is_err() {
                    // ...otherwise, we need to wait for the other worker to finish unlocking.
                    backoff.snooze();
                    continue
                }

                // `self.len` is the absolute total length, while we want an index relative to the current tail
                // fragment's starting index. This is done by reading `preceding` and subtracting `len` with it.
                let preceding = *preceding_len;
                let data = &raw mut (*(*tail).as_ptr()).data;

                if likely(data.len() >= new_len - preceding) {
                    // Immediately release the lock, because the fragment fits and the data slice we requested is
                    // guaranteed not to be aliased by this atomic store.
                    self.len.store(new_len, Release);
                    break ((data as *mut T).add(len - preceding), len)
                } else {
                    // Try to allocate a new fragment and linking to it, releasing the lock as soon as possible.
                    break new_fragment(len, additional, preceding, preceding_len, &self.len, tail)
                }
            }
        };

        #[cold]
        unsafe fn new_fragment<T>(
            len: usize,
            additional: usize,
            preceding: usize,
            this_preceding_len: *mut usize,
            this_len: &AtomicUsize,
            this_tail: *mut NonNull<Fragment<T>>,
        ) -> (*mut T, usize) {
            // First, allocate a new fragment that fits...
            let (new_tail, new_data) = Fragment::<T>::new(len + additional).unwrap_or_else_likely(|_| {
                this_len.store(len, Release);
                panic!("couldn't allocate a fragment")
            });

            unsafe {
                // ...then, write `preceding_len` to the current length and `tail` to the new pointer.
                this_preceding_len.replace(len);
                let tail = this_tail.replace(new_tail).as_ptr();

                // Immediately release the lock so other threads can continue working.
                this_len.store(len + additional, Release);

                // These fields aren't written within the lock because we know subsequent operations will not ever
                // access this fragment in particular, therefore eliminating mutable aliasing.
                (&raw mut (*tail).next).write(new_tail.as_ptr());
                (&raw mut (*tail).len).write(len - preceding)
            }

            (new_data, len)
        }
    }

    /// Extends the vector by `additional` elements, invoking a closure with an uninitialized slice
    /// to it and the starting element's absolute index in the vector.
    ///
    /// # Safety
    /// - `additional` must be less or equal to [`isize::MAX`].
    /// - `acceptor` must fully initialize and not read the given pointer for `additional`
    ///   consecutive items. Notably, this means [`panic!(..)`](panic)-ing mid-way will lead to
    ///   **undefined behavior** due to uninitialized elements present.
    #[inline]
    pub unsafe fn append_raw(&self, additional: usize, acceptor: impl FnOnce(*mut T)) -> usize {
        let (data, index) = unsafe { self.append_raw_erased(additional) };

        acceptor(data);
        index
    }

    /// Appends a slice to this vector, returning the absolute index of the first element.
    #[inline]
    pub fn append(&self, transfer: impl Transfer<T>) -> usize {
        let len = transfer.len();
        if unlikely(len & FLAG == FLAG) {
            panic!("too many elements")
        }

        unsafe { self.append_raw(len, |ptr| transfer.transfer(len, ptr)) }
    }

    /// Flattens the vector (if it isn't flattened already), invokes a closure with an owned slice,
    /// and clears the vector.
    #[inline]
    pub fn clear<'a, R>(&'a mut self, consumer: impl FnOnce(ConsumeSlice<'a, T>) -> R) -> R {
        // First, get and replace both `preceding` and `current` to `0`, marking the vector empty.
        let preceding = core::mem::replace(self.preceding_len.get_mut(), 0);
        let current = core::mem::replace(self.len.get_mut(), 0);

        // If the head and tail points to the same fragment, then the vector is already flattened.
        let head = self.head.as_ptr();
        let slice = if likely(addr_eq(head, self.tail.get_mut().as_ptr())) {
            slice_from_raw_parts_mut(unsafe { &raw mut (*head).data as *mut T }, current)
        } else {
            unsafe { merge(&mut self.head, self.tail.get_mut(), preceding, current) }
        };

        return consumer(ConsumeSlice {
            slice,
            _marker: PhantomData,
        });

        #[cold]
        unsafe fn merge<T>(
            head: &mut NonNull<Fragment<T>>,
            tail: &mut NonNull<Fragment<T>>,
            preceding: usize,
            current: usize,
        ) -> *mut [T] {
            // First, allocate a fragment that fits the current total length...
            let (new_head, mut new_data) =
                Fragment::<T>::new(current).unwrap_or_else_likely(|_| panic!("couldn't allocate a fragment"));

            // ...then, replace the head and tail pointer with this new fragment...
            let start_data = new_data;
            let mut node = core::mem::replace(head, new_head).as_ptr();
            *tail = new_head;

            // ...and finally, collect and deallocate all fragments.
            loop {
                unsafe {
                    let next = (*node).next;
                    let data = &raw const (*node).data;

                    // `next.is_null()` implies `append()` hasn't written to `node->len` yet, so we have to rely on the
                    // current total length subtracted the preceding fragments' total length.
                    let len = if next.is_null() { current - preceding } else { (*node).len };

                    // Copy the data to the new fragment without dropping anything.
                    new_data.copy_from_nonoverlapping(data as *const T, len);
                    new_data = new_data.add(len);

                    // Deallocate the fragment. `unwrap_unchecked()` is safe here, because if a fragment has been
                    // allocated by such a layout, then the same layout may be created again with no issue.
                    dealloc(node as *mut u8, Fragment::<T>::layout(data.len()).unwrap_unchecked());
                    node = match next {
                        // If this is the last fragment, return the fully initialized memory slice ready to be used.
                        ptr if ptr.is_null() => break slice_from_raw_parts_mut(start_data, current),
                        ptr => ptr,
                    };
                }
            }
        }
    }
}

impl<T> Drop for VecBelt<T> {
    fn drop(&mut self) {
        let preceeding = *self.preceding_len.get_mut();
        let current = *self.len.get_mut();
        let mut node = self.head.as_ptr();

        loop {
            let next = unsafe { (*node).next };
            let data = unsafe { &raw mut (*node).data };

            // `next.is_null()` implies `append()` hasn't written to `node->len` yet, so we have to rely on the
            // current total length subtracted the preceeding fragments' total length.
            let taken = if next.is_null() { current - preceeding } else { unsafe { (*node).len } };

            unsafe {
                // Drop `taken` elements starting from the first index in the fragment, as the rest of the data are
                // uninitialized.
                slice_from_raw_parts_mut(data as *mut T, taken).drop_in_place();

                // Deallocate the fragment. `unwrap_unchecked()` is safe here, because if a fragment has been
                // allocated by such a layout, then the same layout may be created again with no issue.
                dealloc(node as *mut u8, Fragment::<T>::layout(data.len()).unwrap_unchecked());
            }

            node = match next {
                ptr if ptr.is_null() => break,
                ptr => ptr,
            };
        }
    }
}

/// Types that may be passed to [`append`](VecBelt::append).
///
/// This trait is implemented on collections that coerce into a slice (`[T]` or `&[T] where T:
/// Copy`), which notably don't include most iterator adapters. This is because
/// [`append`](VecBelt::append) requires all transfer operations to not diverge (e.g.
/// [`panic!(..)`](panic)), otherwise the fragment slice may end up with uninitialized memory which
/// will lead to **undefined behavior** when used or even dropped.
///
/// # Safety
/// - [`len`](Transfer::len) must return **precisely** how much elements this slice will copy. In
///   particular, the returned value will by passed to [`transfer`](Transfer::transfer).
/// - [`transfer`](Transfer::transfer) must write exactly `len` initialized elements to `dst`, and
///   must **not diverge**.
#[allow(clippy::len_without_is_empty)]
pub unsafe trait Transfer<T> {
    /// Returns the length of the slice.
    fn len(&self) -> usize;

    /// Transfer contents of this slice into the destination pointer.
    ///
    /// # Safety
    /// - `len` must be the same as what [`len`](Transfer::len) returns.
    /// - `dst` must be a pointer valid for writing `len` elements, and must not overlap this slice.
    unsafe fn transfer(self, len: usize, dst: *mut T);
}

unsafe impl<T, const LEN: usize> Transfer<T> for [T; LEN] {
    #[inline]
    fn len(&self) -> usize {
        LEN
    }

    #[inline]
    unsafe fn transfer(self, len: usize, dst: *mut T) {
        unsafe { dst.copy_from_nonoverlapping(self.as_ptr(), len) }
        core::mem::forget(self)
    }
}

unsafe impl<T, const LEN: usize> Transfer<T> for core::array::IntoIter<T, LEN> {
    #[inline]
    fn len(&self) -> usize {
        LEN
    }

    #[inline]
    unsafe fn transfer(self, len: usize, dst: *mut T) {
        unsafe { dst.copy_from_nonoverlapping(self.as_slice().as_ptr(), len) }
        self.for_each(core::mem::forget)
    }
}

unsafe impl<T> Transfer<T> for Box<[T]> {
    #[inline]
    fn len(&self) -> usize {
        <[T]>::len(self)
    }

    #[inline]
    unsafe fn transfer(self, len: usize, dst: *mut T) {
        let ptr = Box::into_raw(self);

        unsafe {
            dst.copy_from_nonoverlapping(ptr as *const T, len);
            _ = Box::from_raw(ptr as *mut MaybeUninit<T>)
        }
    }
}

unsafe impl<T> Transfer<T> for Vec<T> {
    #[inline]
    fn len(&self) -> usize {
        <Vec<T>>::len(self)
    }

    #[inline]
    unsafe fn transfer(mut self, len: usize, dst: *mut T) {
        unsafe {
            dst.copy_from_nonoverlapping(self.as_ptr(), len);
            self.set_len(0)
        }
    }
}

unsafe impl<T> Transfer<T> for alloc::vec::IntoIter<T> {
    #[inline]
    fn len(&self) -> usize {
        <Self as ExactSizeIterator>::len(self)
    }

    #[inline]
    unsafe fn transfer(self, len: usize, dst: *mut T) {
        unsafe {
            dst.copy_from_nonoverlapping(self.as_slice().as_ptr(), len);
            self.for_each(core::mem::forget)
        }
    }
}

unsafe impl<T> Transfer<T> for alloc::vec::Drain<'_, T> {
    #[inline]
    fn len(&self) -> usize {
        <Self as ExactSizeIterator>::len(self)
    }

    #[inline]
    unsafe fn transfer(self, len: usize, dst: *mut T) {
        unsafe {
            dst.copy_from_nonoverlapping(self.as_slice().as_ptr(), len);
            self.for_each(core::mem::forget)
        }
    }
}

unsafe impl<T: Copy> Transfer<T> for &[T] {
    #[inline]
    fn len(&self) -> usize {
        <[T]>::len(self)
    }

    #[inline]
    unsafe fn transfer(self, len: usize, dst: *mut T) {
        unsafe { dst.copy_from_nonoverlapping(self.as_ptr(), len) }
    }
}

unsafe impl<T: Copy> Transfer<T> for core::slice::Iter<'_, T> {
    #[inline]
    fn len(&self) -> usize {
        <Self as ExactSizeIterator>::len(self)
    }

    #[inline]
    unsafe fn transfer(self, len: usize, dst: *mut T) {
        unsafe { dst.copy_from_nonoverlapping(self.as_slice().as_ptr(), len) }
    }
}

/// Create a manually-implemented [`Transfer`] with a length and a transferer closure.
///
/// # Safety
/// See the safety documents of [`Transfer`].
#[inline]
pub unsafe fn transfer<T>(len: usize, transfer: impl FnOnce(*mut T)) -> impl Transfer<T> {
    struct Driver<T, F: FnOnce(*mut T)>(usize, F, PhantomData<fn(*mut T)>);
    unsafe impl<T, F: FnOnce(*mut T)> Transfer<T> for Driver<T, F> {
        #[inline]
        fn len(&self) -> usize {
            self.0
        }

        #[inline]
        unsafe fn transfer(self, _: usize, dst: *mut T) {
            self.1(dst)
        }
    }

    Driver(len, transfer, PhantomData)
}

/// An owned slice created by [`VecBelt::append`]. Unconsumed elements will be dropped.
pub struct ConsumeSlice<'a, T> {
    /// Slice returned by [`VecBelt::append_raw`].
    slice: *mut [T],
    _marker: PhantomData<&'a mut [T]>,
}

impl<T> Drop for ConsumeSlice<'_, T> {
    #[inline]
    fn drop(&mut self) {
        unsafe { self.slice.drop_in_place() }
    }
}

impl<T> Deref for ConsumeSlice<'_, T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.slice }
    }
}

impl<T> DerefMut for ConsumeSlice<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.slice }
    }
}

impl<'a, T> IntoIterator for ConsumeSlice<'a, T> {
    type Item = T;
    type IntoIter = ConsumeIter<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        let this = ManuallyDrop::new(self);

        let len = this.slice.len();
        let begin = this.slice as *mut T;
        let end = unsafe { begin.add(len) };

        ConsumeIter {
            begin: unsafe { NonNull::new_unchecked(begin) },
            end: unsafe { NonNull::new_unchecked(end) },
            _marker: PhantomData,
        }
    }
}

/// Owned iterator created by [`ConsumeSlice`] that may be used to read owned elements from the
/// slice.
pub struct ConsumeIter<'a, T> {
    begin: NonNull<T>,
    end: NonNull<T>,
    _marker: PhantomData<&'a mut [T]>,
}

impl<T> ConsumeIter<'_, T> {
    /// Returns the remaining immutable slice iterated by this iterator.
    #[inline]
    pub const fn as_slice(&self) -> &[T] {
        unsafe { &*slice_from_raw_parts(self.begin.as_ptr(), self.end.offset_from(self.begin) as usize) }
    }

    /// Returns the remaining mutable slice iterated by this iterator.
    #[inline]
    pub const fn as_slice_mut(&mut self) -> &mut [T] {
        unsafe { &mut *slice_from_raw_parts_mut(self.begin.as_ptr(), self.end.offset_from(self.begin) as usize) }
    }

    /// Returns the remaining length of the slice iterated by this iterator.
    #[inline]
    pub const fn slice_len(&self) -> usize {
        unsafe { self.end.offset_from(self.begin) as usize }
    }
}

impl<T> Iterator for ConsumeIter<'_, T> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.slice_len() == 0 {
            return None;
        }

        unsafe {
            let item = self.begin.read();
            self.begin = self.begin.add(1);

            Some(item)
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.slice_len();
        (len, Some(len))
    }
}

impl<T> DoubleEndedIterator for ConsumeIter<'_, T> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.slice_len() == 0 {
            return None;
        }

        unsafe {
            self.end = self.end.sub(1);
            Some(self.end.read())
        }
    }
}

impl<T> ExactSizeIterator for ConsumeIter<'_, T> {
    #[inline]
    fn len(&self) -> usize {
        self.slice_len()
    }
}

impl<T> Drop for ConsumeIter<'_, T> {
    #[inline]
    fn drop(&mut self) {
        unsafe { slice_from_raw_parts_mut(self.begin.as_ptr(), self.end.offset_from(self.begin) as usize).drop_in_place() }
    }
}

impl<'a, T> IntoIterator for &'a ConsumeSlice<'a, T> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut ConsumeSlice<'a, T> {
    type Item = &'a mut T;
    type IntoIter = core::slice::IterMut<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

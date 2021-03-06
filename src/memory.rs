#[allow(unused_imports)]
use alloc::prelude::v1::*;
use alloc::rc::Rc;
use core::{
    cell::{Cell, RefCell},
    cmp, fmt, slice,
    ops::{Range, Deref, DerefMut},
    u32,
};
use memory_units::{Bytes, Pages, RoundUpTo};
use parity_wasm::elements::ResizableLimits;
use value::LittleEndianConvert;
use Error;

/// Size of a page of [linear memory][`MemoryInstance`] - 64KiB.
///
/// The size of a memory is always a integer multiple of a page size.
///
/// [`MemoryInstance`]: struct.MemoryInstance.html
pub const LINEAR_MEMORY_PAGE_SIZE: Bytes = Bytes(65536);

/// Reference to a memory (See [`MemoryInstance`] for details).
///
/// This reference has a reference-counting semantics.
///
/// [`MemoryInstance`]: struct.MemoryInstance.html
///
#[derive(Clone, Debug)]
pub struct MemoryRef(Rc<MemoryInstance>);

impl ::core::ops::Deref for MemoryRef {
    type Target = MemoryInstance;
    fn deref(&self) -> &MemoryInstance {
        &self.0
    }
}

pub trait Allocator: Deref<Target=[u8]> + DerefMut<Target=[u8]> {
    fn resize(&mut self, usize, value: u8);
}

impl Allocator for Vec<u8> {
    fn resize(&mut self, size: usize, value: u8) {
        Vec::resize(self, size, value)
    }
}

impl Allocator for &'static mut [u8] {
    fn resize(&mut self, _size: usize, _value: u8) {
        // no op
    }
}

/// Runtime representation of a linear memory (or `memory` for short).
///
/// A memory is a contiguous, mutable array of raw bytes. Wasm code can load and store values
/// from/to a linear memory at any byte address.
/// A trap occurs if an access is not within the bounds of the current memory size.
///
/// A memory is created with an initial size but can be grown dynamically.
/// The growth can be limited by specifying maximum size.
/// The size of a memory is always a integer multiple of a [page size][`LINEAR_MEMORY_PAGE_SIZE`] - 64KiB.
///
/// At the moment, wasm doesn't provide any way to shrink the memory.
///
/// [`LINEAR_MEMORY_PAGE_SIZE`]: constant.LINEAR_MEMORY_PAGE_SIZE.html
pub struct MemoryInstance {
    /// Memory limits.
    limits: ResizableLimits,
    /// Linear memory buffer with lazy allocation.
    buffer: RefCell<Box<dyn Allocator>>,
    initial: Pages,
    current_size: Cell<usize>,
    maximum: Option<Pages>,
    lowest_used: Cell<u32>,
    buffer_ptr: Cell<*mut u8>,
    buffer_size: Cell<usize>,
}

impl fmt::Debug for MemoryInstance {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MemoryInstance")
            .field("limits", &self.limits)
            .field("buffer.len", &self.buffer.borrow().len())
            .field("maximum", &self.maximum)
            .field("initial", &self.initial)
            .finish()
    }
}

struct CheckedRegion {
    offset: usize,
    size: usize,
}

impl CheckedRegion {
    fn range(&self) -> Range<usize> {
        self.offset..self.offset + self.size
    }

    fn intersects(&self, other: &Self) -> bool {
        let low = cmp::max(self.offset, other.offset);
        let high = cmp::min(self.offset + self.size, other.offset + other.size);

        low < high
    }
}

impl MemoryInstance {
    /// Allocate a memory instance.
    ///
    /// The memory allocated with initial number of pages specified by `initial`.
    /// Minimal possible value for `initial` is 0 and maximum possible is `65536`.
    /// (Since maximum addressible memory is 2<sup>32</sup> = 4GiB = 65536 * [64KiB][`LINEAR_MEMORY_PAGE_SIZE`]).
    ///
    /// It is possible to limit maximum number of pages this memory instance can have by specifying
    /// `maximum`. If not specified, this memory instance would be able to allocate up to 4GiB.
    ///
    /// Allocated memory is always zeroed.
    ///
    /// # Errors
    ///
    /// Returns `Err` if:
    ///
    /// - `initial` is greater than `maximum`
    /// - either `initial` or `maximum` is greater than `65536`.
    ///
    /// [`LINEAR_MEMORY_PAGE_SIZE`]: constant.LINEAR_MEMORY_PAGE_SIZE.html
    pub fn alloc(initial: Pages, maximum: Option<Pages>) -> Result<MemoryRef, Error> {
        {
            use core::convert::TryInto;
            let initial_u32: u32 = initial.0.try_into().map_err(|_| {
                Error::Memory(format!("initial ({}) can't be coerced to u32", initial.0))
            })?;
            let maximum_u32: Option<u32> = match maximum {
                Some(maximum_pages) => Some(maximum_pages.0.try_into().map_err(|_| {
                    Error::Memory(format!(
                        "maximum ({}) can't be coerced to u32",
                        maximum_pages.0
                    ))
                })?),
                None => None,
            };
            validation::validate_memory(initial_u32, maximum_u32).map_err(Error::Memory)?;
        }

        let allocator = Box::new(Vec::with_capacity(4096));
        let memory = MemoryInstance::new(initial, maximum, allocator);
        Ok(MemoryRef(Rc::new(memory)))
    }

    /// Create a memory instance using specified raw memory. The memory address must
    /// be aligned to a page size. The size must be a multiple of page size.
    ///
    /// # Errors
    ///
    /// Returns `Err` if:
    ///
    /// - `buffer` is not aligned to page size.
    /// - `size` is not a multiple of page size.
    pub fn with_memory(buffer: *mut u8, size: usize) -> Result<MemoryRef, Error> {
        if (buffer as usize) % LINEAR_MEMORY_PAGE_SIZE.0 != 0 {
            return Err(Error::Memory(format!(
                "Buffer address must be aligned to page size",
            )))
        }

        if size % LINEAR_MEMORY_PAGE_SIZE.0 != 0 {
            return Err(Error::Memory(format!(
                "Size {} must be multiple of page size",
                size,
            )))
        }

        let pages: Pages = Bytes(size).round_up_to();
        if pages > Pages(validation::LINEAR_MEMORY_MAX_PAGES as usize) {
            return Err(Error::Memory(format!(
                "Memory size must be at most {} pages",
                validation::LINEAR_MEMORY_MAX_PAGES
            )));
        }
        let allocator = unsafe { Box::new(slice::from_raw_parts_mut(buffer, size)) };
        let memory = MemoryInstance::new(pages, Some(pages), allocator);
        Ok(MemoryRef(Rc::new(memory)))
    }

    /// Create new linear memory instance.
    fn new(initial: Pages, maximum: Option<Pages>, mut allocator: Box<Allocator>) -> Self {
        let limits = ResizableLimits::new(initial.0 as u32, maximum.map(|p| p.0 as u32));

        let initial_size: Bytes = initial.into();
        let ptr = allocator.as_mut_ptr();
        MemoryInstance {
            limits: limits,
            buffer: RefCell::new(allocator),
            initial: initial,
            current_size: Cell::new(initial_size.0),
            maximum: maximum,
            lowest_used: Cell::new(u32::max_value()),
            buffer_ptr: Cell::new(ptr),
            buffer_size: Cell::new(0),
        }
    }

    /// Return linear memory limits.
    pub(crate) fn limits(&self) -> &ResizableLimits {
        &self.limits
    }

    /// Returns number of pages this `MemoryInstance` was created with.
    pub fn initial(&self) -> Pages {
        self.initial
    }

    /// Returns maximum amount of pages this `MemoryInstance` can grow to.
    ///
    /// Returns `None` if there is no limit set.
    /// Maximum memory size cannot exceed `65536` pages or 4GiB.
    pub fn maximum(&self) -> Option<Pages> {
        self.maximum
    }

    /// Returns lowest offset ever written or `u32::max_value()` if none.
    pub fn lowest_used(&self) -> u32 {
        self.lowest_used.get()
    }

    /// Resets tracked lowest offset.
    pub fn reset_lowest_used(&self, addr: u32) {
        self.lowest_used.set(addr)
    }

    /// Returns current linear memory size.
    ///
    /// Maximum memory size cannot exceed `65536` pages or 4GiB.
    ///
    /// # Example
    ///
    /// To convert number of pages to number of bytes you can use the following code:
    ///
    /// ```rust
    /// use wasmi::MemoryInstance;
    /// use wasmi::memory_units::*;
    ///
    /// let memory = MemoryInstance::alloc(Pages(1), None).unwrap();
    /// let byte_size: Bytes = memory.current_size().into();
    /// assert_eq!(
    ///     byte_size,
    ///     Bytes(65536),
    /// );
    /// ```
    pub fn current_size(&self) -> Pages {
        Bytes(self.current_size.get()).round_up_to()
    }

    /// Returns current used memory size in bytes.
    /// This is one more than the highest memory address that had been written to.
    pub fn used_size(&self) -> Bytes {
        Bytes(self.buffer_size.get())
    }

    /// Get value from memory at given offset.
    pub fn get_value<T: LittleEndianConvert>(&self, offset: u32) -> Result<T, Error> {
        let region =
            self.checked_region(offset as usize, ::core::mem::size_of::<T>())?;
        let mem = unsafe {  slice::from_raw_parts_mut(self.buffer_ptr.get(), self.buffer_size.get()) };
        Ok(T::from_little_endian(&mem[region.range()]).expect("Slice size is checked"))
    }

    /// Copy data from memory at given offset.
    ///
    /// This will allocate vector for you.
    /// If you can provide a mutable slice you can use [`get_into`].
    ///
    /// [`get_into`]: #method.get_into
    pub fn get(&self, offset: u32, size: usize) -> Result<Vec<u8>, Error> {
        let region = self.checked_region(offset as usize, size)?;
        let mem = unsafe { slice::from_raw_parts_mut(self.buffer_ptr.get(), self.buffer_size.get()) };
        Ok(mem[region.range()].to_vec())
    }

    /// Copy data from given offset in the memory into `target` slice.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the specified region is out of bounds.
    pub fn get_into(&self, offset: u32, target: &mut [u8]) -> Result<(), Error> {
        let region = self.checked_region(offset as usize, target.len())?;
        let mem = unsafe {  slice::from_raw_parts_mut(self.buffer_ptr.get(), self.buffer_size.get()) };
        target.copy_from_slice(&mem[region.range()]);

        Ok(())
    }

    /// Copy data in the memory at given offset.
    pub fn set(&self, offset: u32, value: &[u8]) -> Result<(), Error> {
        let range = self
            .checked_region(offset as usize, value.len())?
            .range();

        if offset < self.lowest_used.get() {
            self.lowest_used.set(offset);
        }
        let mem = unsafe { slice::from_raw_parts_mut(self.buffer_ptr.get(), self.buffer_size.get()) };
        mem[range].copy_from_slice(value);
        Ok(())
    }

    /// Copy value in the memory at given offset.
    pub fn set_value<T: LittleEndianConvert>(&self, offset: u32, value: T) -> Result<(), Error> {
        let range = self
            .checked_region(offset as usize, ::core::mem::size_of::<T>())?
            .range();
        if offset < self.lowest_used.get() {
            self.lowest_used.set(offset);
        }
        let mem = unsafe { slice::from_raw_parts_mut(self.buffer_ptr.get(), self.buffer_size.get()) };
        value.into_little_endian(&mut mem[range]);
        Ok(())
    }

    /// Increases the size of the linear memory by given number of pages.
    /// Returns previous memory size if succeeds.
    ///
    /// # Errors
    ///
    /// Returns `Err` if attempted to allocate more memory than permited by the limit.
    pub fn grow(&self, additional: Pages) -> Result<Pages, Error> {
        let size_before_grow: Pages = self.current_size();

        if additional == Pages(0) {
            return Ok(size_before_grow);
        }
        if additional > Pages(65536) {
            return Err(Error::Memory(format!(
                "Trying to grow memory by more than 65536 pages"
            )));
        }

        let new_size: Pages = size_before_grow + additional;
        let maximum = self
            .maximum
            .unwrap_or(Pages(validation::LINEAR_MEMORY_MAX_PAGES as usize));
        if new_size > maximum {
            return Err(Error::Memory(format!(
                "Trying to grow memory by {} pages when already have {}",
                additional.0, size_before_grow.0,
            )));
        }

        let new_buffer_length: Bytes = new_size.into();
        self.current_size.set(new_buffer_length.0);
        Ok(size_before_grow)
    }

    fn checked_region(
        &self,
        offset: usize,
        size: usize,
    ) -> Result<CheckedRegion, Error> {
        let end = offset.checked_add(size).ok_or_else(|| {
            Error::Memory(format!(
                "trying to access memory block of size {} from offset {}",
                size, offset
            ))
        })?;

        if end <= self.current_size.get() && self.buffer_size.get() < end {
            let mut allocator = self.buffer.borrow_mut();
            allocator.resize(end, 0);
            self.buffer_ptr.set(allocator.as_mut_ptr());
            self.buffer_size.set(allocator.len());
        }

        if end > self.buffer_size.get() {
            return Err(Error::Memory(format!(
                "trying to access region [{}..{}] in memory [0..{}]",
                offset,
                end,
                self.buffer_size.get(),
            )));
        }

        Ok(CheckedRegion {
            offset: offset,
            size: size,
        })
    }

    fn checked_region_pair(
        &self,
        offset1: usize,
        size1: usize,
        offset2: usize,
        size2: usize,
    ) -> Result<(CheckedRegion, CheckedRegion), Error> {
        let end1 = offset1.checked_add(size1).ok_or_else(|| {
            Error::Memory(format!(
                "trying to access memory block of size {} from offset {}",
                size1, offset1
            ))
        })?;

        let end2 = offset2.checked_add(size2).ok_or_else(|| {
            Error::Memory(format!(
                "trying to access memory block of size {} from offset {}",
                size2, offset2
            ))
        })?;

        let max = cmp::max(end1, end2);
        if max <= self.current_size.get() && self.buffer_size.get() < max {
            let mut allocator = self.buffer.borrow_mut();
            allocator.resize(max, 0);
            self.buffer_ptr.set(allocator.as_mut_ptr());
            self.buffer_size.set(allocator.len());
        }

        if end1 > self.buffer_size.get() {
            return Err(Error::Memory(format!(
                "trying to access region [{}..{}] in memory [0..{}]",
                offset1,
                end1,
                self.buffer_size.get(),
            )));
        }

        if end2 > self.buffer_size.get() {
            return Err(Error::Memory(format!(
                "trying to access region [{}..{}] in memory [0..{}]",
                offset2,
                end2,
                self.buffer_size.get(),
            )));
        }

        Ok((
            CheckedRegion {
                offset: offset1,
                size: size1,
            },
            CheckedRegion {
                offset: offset2,
                size: size2,
            },
        ))
    }

    /// Copy contents of one memory region to another.
    ///
    /// Semantically equivalent to `memmove`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if either of specified regions is out of bounds.
    pub fn copy(&self, src_offset: usize, dst_offset: usize, len: usize) -> Result<(), Error> {
        let (read_region, write_region) =
            self.checked_region_pair(src_offset, len, dst_offset, len)?;

        if dst_offset < self.lowest_used.get() as usize {
            self.lowest_used.set(dst_offset as u32);
        }

        unsafe {
            ::core::ptr::copy(
                self.buffer_ptr.get().offset(read_region.offset as isize),
                self.buffer_ptr.get().offset(write_region.offset as isize),
                len,
            )
        }

        Ok(())
    }

    /// Copy contents of one memory region to another (non-overlapping version).
    ///
    /// Semantically equivalent to `memcpy`.
    /// but returns Error if source overlaping with destination.
    ///
    /// # Errors
    ///
    /// Returns `Err` if:
    ///
    /// - either of specified regions is out of bounds,
    /// - these regions overlaps.
    pub fn copy_nonoverlapping(
        &self,
        src_offset: usize,
        dst_offset: usize,
        len: usize,
    ) -> Result<(), Error> {

        let (read_region, write_region) =
            self.checked_region_pair(src_offset, len, dst_offset, len)?;

        if read_region.intersects(&write_region) {
            return Err(Error::Memory(format!(
                "non-overlapping copy is used for overlapping regions"
            )));
        }

        if dst_offset < self.lowest_used.get() as usize {
            self.lowest_used.set(dst_offset as u32);
        }

        unsafe {
            ::core::ptr::copy_nonoverlapping(
                self.buffer_ptr.get().offset(read_region.offset as isize),
                self.buffer_ptr.get().offset(write_region.offset as isize),
                len,
            )
        }

        Ok(())
    }

    /// Copy memory between two (possibly distinct) memory instances.
    ///
    /// If the same memory instance passed as `src` and `dst` then usual `copy` will be used.
    pub fn transfer(
        src: &MemoryRef,
        src_offset: usize,
        dst: &MemoryRef,
        dst_offset: usize,
        len: usize,
    ) -> Result<(), Error> {
        if Rc::ptr_eq(&src.0, &dst.0) {
            // `transfer` is invoked with with same source and destination. Let's assume that regions may
            // overlap and use `copy`.
            return src.copy(src_offset, dst_offset, len);
        }

        let src_range = src
            .checked_region(src_offset, len)?
            .range();
        let dst_range = dst
            .checked_region(dst_offset, len)?
            .range();

        if dst_offset < dst.lowest_used.get() as usize {
            dst.lowest_used.set(dst_offset as u32);
        }

        unsafe {
            ::core::ptr::copy_nonoverlapping(
                src.buffer_ptr.get().offset(src_range.start as isize),
                dst.buffer_ptr.get().offset(dst_range.start as isize),
                len,
                )
        }

        Ok(())
    }

    /// Fill the memory region with the specified value.
    ///
    /// Semantically equivalent to `memset`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the specified region is out of bounds.
    pub fn clear(&self, offset: usize, new_val: u8, len: usize) -> Result<(), Error> {
        let range = self.checked_region(offset, len)?.range();

        if offset < self.lowest_used.get() as usize {
            self.lowest_used.set(offset as u32);
        }

        unsafe {
            ::core::ptr::write_bytes(
                self.buffer_ptr.get().offset(range.start as isize),
                new_val,
                len,
            );
        }
        Ok(())
    }

    /// Fill the specified memory region with zeroes.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the specified region is out of bounds.
    pub fn zero(&self, offset: usize, len: usize) -> Result<(), Error> {
        self.clear(offset, 0, len)
    }

    /// Provides direct access to the underlying memory buffer.
    ///
    /// # Panics
    ///
    /// Any call that requires write access to memory (such as [`set`], [`clear`], etc) made within
    /// the closure will panic. Note that the buffer size may be arbitraty. Proceed with caution.
    ///
    /// [`set`]: #method.get
    /// [`clear`]: #method.set
    pub fn with_direct_access<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
        let buf = self.buffer.borrow();
        f(&*buf)
    }

    /// Provides direct mutable access to the underlying memory buffer.
    ///
    /// # Panics
    ///
    /// Any calls that requires either read or write access to memory (such as [`get`], [`set`], [`copy`], etc) made
    /// within the closure will panic. Note that the buffer size may be arbitraty.
    /// The closure may however resize it. Proceed with caution.
    ///
    /// [`get`]: #method.get
    /// [`set`]: #method.set
    /// [`copy`]: #method.copy
    pub fn with_direct_access_mut<R, F: FnOnce(&mut Allocator) -> R>(&self, f: F) -> R {
        let mut buf = self.buffer.borrow_mut();
        f(&mut **buf)
    }
}

#[cfg(test)]
mod tests {

    use super::{MemoryInstance, MemoryRef, LINEAR_MEMORY_PAGE_SIZE};
    use memory_units::Pages;
    use std::rc::Rc;
    use Error;

    #[test]
    fn alloc() {
        #[cfg(target_pointer_width = "64")]
        let fixtures = &[
            (0, None, true),
            (0, Some(0), true),
            (1, None, true),
            (1, Some(1), true),
            (0, Some(1), true),
            (1, Some(0), false),
            (0, Some(65536), true),
            (65536, Some(65536), true),
            (65536, Some(0), false),
            (65536, None, true),
        ];

        #[cfg(target_pointer_width = "32")]
        let fixtures = &[
            (0, None, true),
            (0, Some(0), true),
            (1, None, true),
            (1, Some(1), true),
            (0, Some(1), true),
            (1, Some(0), false),
        ];

        for (index, &(initial, maybe_max, expected_ok)) in fixtures.iter().enumerate() {
            let initial: Pages = Pages(initial);
            let maximum: Option<Pages> = maybe_max.map(|m| Pages(m));
            let result = MemoryInstance::alloc(initial, maximum);
            if result.is_ok() != expected_ok {
                panic!(
                    "unexpected error at {}, initial={:?}, max={:?}, expected={}, result={:?}",
                    index, initial, maybe_max, expected_ok, result,
                );
            }
        }
    }

    #[test]
    fn ensure_page_size() {
        use memory_units::ByteSize;
        assert_eq!(LINEAR_MEMORY_PAGE_SIZE, Pages::byte_size());
    }

    fn create_memory(initial_content: &[u8]) -> MemoryInstance {
        let mem = MemoryInstance::new(Pages(1), Some(Pages(1)), Box::new(Vec::new()));
        mem.set(0, initial_content)
            .expect("Successful initialize the memory");
        mem
    }

    #[test]
    fn copy_overlaps_1() {
        let mem = create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        mem.copy(0, 4, 6).expect("Successfully copy the elements");
        let result = mem.get(0, 10).expect("Successfully retrieve the result");
        assert_eq!(result, &[0, 1, 2, 3, 0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn copy_overlaps_2() {
        let mem = create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        mem.copy(4, 0, 6).expect("Successfully copy the elements");
        let result = mem.get(0, 10).expect("Successfully retrieve the result");
        assert_eq!(result, &[4, 5, 6, 7, 8, 9, 6, 7, 8, 9]);
    }

    #[test]
    fn copy_nonoverlapping() {
        let mem = create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        mem.copy_nonoverlapping(0, 10, 10)
            .expect("Successfully copy the elements");
        let result = mem.get(10, 10).expect("Successfully retrieve the result");
        assert_eq!(result, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn copy_nonoverlapping_overlaps_1() {
        let mem = create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let result = mem.copy_nonoverlapping(0, 4, 6);
        match result {
            Err(Error::Memory(_)) => {}
            _ => panic!("Expected Error::Memory(_) result, but got {:?}", result),
        }
    }

    #[test]
    fn copy_nonoverlapping_overlaps_2() {
        let mem = create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let result = mem.copy_nonoverlapping(4, 0, 6);
        match result {
            Err(Error::Memory(_)) => {}
            _ => panic!("Expected Error::Memory(_), but got {:?}", result),
        }
    }

    #[test]
    fn transfer_works() {
        let src = MemoryRef(Rc::new(create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])));
        let dst = MemoryRef(Rc::new(create_memory(&[
            10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
        ])));

        MemoryInstance::transfer(&src, 4, &dst, 0, 3).unwrap();

        assert_eq!(src.get(0, 10).unwrap(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            dst.get(0, 10).unwrap(),
            &[4, 5, 6, 13, 14, 15, 16, 17, 18, 19]
        );
    }

    #[test]
    fn transfer_still_works_with_same_memory() {
        let src = MemoryRef(Rc::new(create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])));

        MemoryInstance::transfer(&src, 4, &src, 0, 3).unwrap();

        assert_eq!(src.get(0, 10).unwrap(), &[4, 5, 6, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn transfer_oob_with_same_memory_errors() {
        let src = MemoryRef(Rc::new(create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])));
        assert!(MemoryInstance::transfer(&src, 65535, &src, 0, 3).is_err());

        // Check that memories content left untouched
        assert_eq!(src.get(0, 10).unwrap(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn transfer_oob_errors() {
        let src = MemoryRef(Rc::new(create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])));
        let dst = MemoryRef(Rc::new(create_memory(&[
            10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
        ])));

        assert!(MemoryInstance::transfer(&src, 65535, &dst, 0, 3).is_err());

        // Check that memories content left untouched
        assert_eq!(src.get(0, 10).unwrap(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            dst.get(0, 10).unwrap(),
            &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19]
        );
    }

    #[test]
    fn clear() {
        let mem = create_memory(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        mem.clear(0, 0x4A, 10)
            .expect("To successfully clear the memory");
        let result = mem.get(0, 10).expect("To successfully retrieve the result");
        assert_eq!(result, &[0x4A; 10]);
    }

    #[test]
    fn get_into() {
        let mem = MemoryInstance::new(Pages(1), None, Box::new(Vec::new()));
        mem.set(6, &[13, 17, 129])
            .expect("memory set should not fail");

        let mut data = [0u8; 2];
        mem.get_into(7, &mut data[..])
            .expect("get_into should not fail");

        assert_eq!(data, [17, 129]);
    }

    #[test]
    fn zero_copy() {
        let mem = MemoryInstance::alloc(Pages(1), None).unwrap();
        mem.set(100, &[0]).expect("memory set should not fail");
        mem.with_direct_access_mut(|buf| {
            assert_eq!(buf.len(), 101);
            buf[..10].copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        });
        mem.with_direct_access(|buf| {
            assert_eq!(buf.len(), 101);
            assert_eq!(&buf[..10], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        });
    }

    #[should_panic]
    #[test]
    fn zero_copy_panics_on_nested_access() {
        let mem = MemoryInstance::alloc(Pages(1), None).unwrap();
        let mem_inner = mem.clone();
        mem.with_direct_access(move |_| {
            let _ = mem_inner.set(0, &[11, 12, 13]);
        });
    }
}

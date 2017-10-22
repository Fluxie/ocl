
use std::ops::{Deref, DerefMut};
use futures::{Future, Poll};
use core::{self, Result as OclResult, OclPrm, MemMap as MemMapCore,
    MemFlags, MapFlags, ClNullEventPtr};
use standard::{Event, EventList, Queue, Buffer};
use async::{Error as AsyncError, OrderLock, FutureGuard, ReadGuard,
    WriteGuard};


/// The future of plumbing! Errr, wait a sec.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct FutureFlush<T: OclPrm> {
    future_guard: FutureGuard<Inner<T>, ReadGuard<Inner<T>>>,
}

impl<T: OclPrm> FutureFlush<T> {
    fn new(future_guard: FutureGuard<Inner<T>, ReadGuard<Inner<T>>>) -> FutureFlush<T> {
        FutureFlush { future_guard: future_guard }
    }
}

impl<T: OclPrm> Future for FutureFlush<T> {
    type Item = ();
    type Error = AsyncError;

    #[inline]
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.future_guard.poll().map(|res| res.map(|_read_guard| ()))
    }
}


#[derive(Debug)]
pub struct Inner<T: OclPrm> {
    buffer: Buffer<T>,
    memory: MemMapCore<T>,
    offset: usize,
    len: usize,
}

impl<T: OclPrm> Inner<T> {
    /// Returns the internal buffer.
    pub fn buffer(self: &Inner<T>) -> &Buffer<T> {
        &self.buffer
    }

    /// Returns the internal memory mapping.
    pub fn memory(self: &Inner<T>) -> &MemMapCore<T> {
        &self.memory
    }

    /// Returns the internal offset.
    pub fn offset(self: &Inner<T>) -> usize {
        self.offset
    }
}

impl<T: OclPrm> Deref for Inner<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        unsafe { self.memory.as_slice(self.len) }
    }
}

impl<T: OclPrm> DerefMut for Inner<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        unsafe { self.memory.as_slice_mut(self.len) }
    }
}

impl<T: OclPrm> Drop for Inner<T> {
    /// Drops the `Inner` enqueuing an unmap and blocking until it
    /// completes.
    fn drop(&mut self) {
        let mut new_event = Event::empty();
        core::enqueue_unmap_mem_object::<T, _, _, _>(self.buffer.default_queue().unwrap(),
            &self.buffer, &self.memory, None::<&EventList>, Some(&mut new_event)).unwrap();
        new_event.wait_for().unwrap();
    }
}


/// Represents mapped memory and allows frames of data to be 'flushed'
/// (written) from host-accessible mapped memory region into its associated
/// device-visible buffer in a repeated fashion.
///
/// This represents the fastest possible method for continuously writing
/// frames of data to a device.
///
#[derive(Clone, Debug)]
pub struct BufferSink<T: OclPrm> {
    lock: OrderLock<Inner<T>>,
}

impl<T: OclPrm> BufferSink<T> {
    /// Returns a new `BufferSink`.
    ///
    /// ## Safety
    ///
    /// `buffer` must not have the same region mapped more than once.
    ///
    pub unsafe fn new(mut buffer: Buffer<T>, queue: Queue, offset: usize, len: usize)
            -> OclResult<BufferSink<T>> {
        // TODO: Ensure that these checks are complete enough.
        let buf_flags = buffer.flags()?;
        assert!(buf_flags.contains(MemFlags::new().alloc_host_ptr()) ||
            buf_flags.contains(MemFlags::new().use_host_ptr()),
            "A buffer sink must be created with a buffer that has either \
            the MEM_ALLOC_HOST_PTR` or `MEM_USE_HOST_PTR flag.");
        assert!(!buf_flags.contains(MemFlags::new().host_no_access()) &&
            !buf_flags.contains(MemFlags::new().host_read_only()),
            "A buffer sink may not be created with a buffer that has either the \
            `MEM_HOST_NO_ACCESS` or `MEM_HOST_READ_ONLY` flags.");

        buffer.set_default_queue(queue);
        let map_flags = MapFlags::new().write_invalidate_region();

        let memory = core::enqueue_map_buffer::<T, _, _, _>(buffer.default_queue().unwrap(),
            buffer.core(), true, map_flags, offset, len, None::<&EventList>, None::<&mut Event>)?;

        let inner = Inner {
            buffer,
            memory,
            offset,
            len
        };

        Ok(BufferSink {
            lock: OrderLock::new(inner),
        })
    }

    /// Returns a new `FutureGuard` which will resolve into a a `ReadGuard`.
    pub fn read(self) -> FutureGuard<Inner<T>, ReadGuard<Inner<T>>> {
        self.lock.read()

    }

    /// Returns a new `FutureGuard` which will resolve into a a `WriteGuard`.
    pub fn write(self) -> FutureGuard<Inner<T>, WriteGuard<Inner<T>>> {
        self.lock.write()
    }

    /// Flushes the mapped memory region to the device.
    pub fn flush<Ewl, En>(&self, wait_events: Option<Ewl>, mut flush_event: Option<En>)
            -> OclResult<FutureFlush<T>>
            where Ewl: Into<EventList>, En: ClNullEventPtr {
        let mut future_read = self.clone().read();
        if let Some(wl) = wait_events {
            future_read.set_wait_list(wl);
        }

        let mut unmap_event = Event::empty();
        let mut map_event = Event::empty();

        unsafe {
            let inner = &*self.lock.as_ptr();
            let queue = inner.buffer.default_queue().unwrap();
            let buffer = inner.buffer.core();

            // Ensure that we have a read lock before the command can run.
            future_read.create_lock_event(queue.context_ptr()?)?;

            core::enqueue_unmap_mem_object::<T, _, _, _>(queue, buffer, &inner.memory,
                future_read.lock_event(), Some(&mut unmap_event))?;

            let map_flags = MapFlags::new().write_invalidate_region();
            core::enqueue_map_buffer::<T, _, _, _>(queue, buffer, false, map_flags,
                inner.offset, inner.len, Some(&unmap_event), Some(&mut map_event))?;
        }

        // Use `map_event` as the tail/conclusion event.
        if let Some(ref mut enew) = flush_event {
            unsafe { enew.clone_from(&map_event) }
        }

        // Ensure that the unmap and (re)map finish.
        future_read.set_command_completion_event(map_event.clone());

        Ok(FutureFlush::new(future_read))
    }

    /// Returns a mutable slice into the contained memory region.
    ///
    /// Used by buffer command builders when preparing future read and write
    /// commands.
    ///
    /// Do not use unless you are 100% certain that there will be no other
    /// reads or writes for the entire access duration (only possible if
    /// manually manipulating the lock status).
    pub unsafe fn as_mut_slice(&self) -> &mut [T] {
        let ptr = (*self.lock.as_mut_ptr()).memory.as_mut_ptr();
        let len = (*self.lock.as_ptr()).len;
        ::std::slice::from_raw_parts_mut(ptr, len)
    }

    /// Returns the length of the memory region.
    pub fn len(&self) -> usize {
        unsafe { (*self.lock.as_ptr()).len }
    }

    /// Returns a pointer address to the internal memory region, usable as a
    /// unique identifier.
    pub fn id(&self) -> usize {
        unsafe { (*self.lock.as_ptr()).memory.as_ptr() as usize }
    }
}

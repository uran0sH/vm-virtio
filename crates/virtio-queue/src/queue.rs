// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright (C) 2020-2021 Alibaba Cloud. All rights reserved.
// Copyright © 2019 Intel Corporation.
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

use std::mem::size_of;
use std::num::Wrapping;
use std::ops::Deref;
use std::sync::atomic::{fence, Ordering};

use vm_memory::{Address, Bytes, GuestAddress, GuestMemory};

use crate::defs::{
    DEFAULT_AVAIL_RING_ADDR, DEFAULT_DESC_TABLE_ADDR, DEFAULT_USED_RING_ADDR,
    VIRTQ_AVAIL_ELEMENT_SIZE, VIRTQ_AVAIL_RING_HEADER_SIZE, VIRTQ_AVAIL_RING_META_SIZE,
    VIRTQ_USED_ELEMENT_SIZE, VIRTQ_USED_RING_HEADER_SIZE, VIRTQ_USED_RING_META_SIZE,
};
use crate::{
    error, AvailIter, Descriptor, DescriptorChain, Error, QueueStateGuard, QueueStateOwnedT,
    QueueStateT, VirtqUsedElem,
};
use virtio_bindings::bindings::virtio_ring::VRING_USED_F_NO_NOTIFY;

/// Struct to maintain information and manipulate a virtio queue.
///
/// # Example
///
/// ```rust
/// use virtio_queue::{Queue, QueueStateOwnedT, QueueStateT};
/// use vm_memory::{Bytes, GuestAddress, GuestAddressSpace, GuestMemoryMmap};
///
/// let m = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
/// let mut queue = Queue::new(1024);
///
/// // First, the driver sets up the queue; this set up is done via writes on the bus (PCI, MMIO).
/// queue.set_size(8);
/// queue.set_desc_table_address(Some(0x1000), None);
/// queue.set_avail_ring_address(Some(0x2000), None);
/// queue.set_used_ring_address(Some(0x3000), None);
/// queue.set_event_idx(true);
/// queue.set_ready(true);
/// // The user should check if the queue is valid before starting to use it.
/// assert!(queue.is_valid(&m));
///
/// // Here the driver would add entries in the available ring and then update the `idx` field of
/// // the available ring (address = 0x2000 + 2).
/// m.write_obj(3, GuestAddress(0x2002));
///
/// loop {
///     queue.disable_notification(&m).unwrap();
///
///     // Consume entries from the available ring.
///     while let Some(chain) = queue.iter(&m).unwrap().next() {
///         // Process the descriptor chain, and then add an entry in the used ring and optionally
///         // notify the driver.
///         queue.add_used(&m, chain.head_index(), 0x100).unwrap();
///
///         if queue.needs_notification(&m).unwrap() {
///             // Here we would notify the driver it has new entries in the used ring to consume.
///         }
///     }
///     if !queue.enable_notification(&m).unwrap() {
///         break;
///     }
/// }
///
/// // We can reset the queue at some point.
/// queue.reset();
/// // The queue should not be ready after reset.
/// assert!(!queue.ready());
/// ```
///
/// WARNING: The current implementation allows setting up and using an invalid queue
/// (initialized with random data since the `Queue`'s fields are public). When fixing
/// <https://github.com/rust-vmm/vm-virtio/issues/172>, we plan to define a `QueueState` that
/// represents the actual state of the queue (no `Wrapping`s in it, for example). This way, we
/// will also be able to do the checks that we normally do in the queue's field setters when
/// starting from scratch, when trying to create a `Queue` from a `QueueState`.
#[derive(Debug, Default, PartialEq)]
pub struct Queue {
    /// The maximum size in elements offered by the device.
    pub max_size: u16,

    /// Tail position of the available ring.
    pub next_avail: Wrapping<u16>,

    /// Head position of the used ring.
    pub next_used: Wrapping<u16>,

    /// VIRTIO_F_RING_EVENT_IDX negotiated.
    pub event_idx_enabled: bool,

    /// The number of descriptor chains placed in the used ring via `add_used`
    /// since the last time `needs_notification` was called on the associated queue.
    pub num_added: Wrapping<u16>,

    /// The queue size in elements the driver selected.
    pub size: u16,

    /// Indicates if the queue is finished with configuration.
    pub ready: bool,

    /// Guest physical address of the descriptor table.
    pub desc_table: GuestAddress,

    /// Guest physical address of the available ring.
    pub avail_ring: GuestAddress,

    /// Guest physical address of the used ring.
    pub used_ring: GuestAddress,
}

impl Queue {
    // Helper method that writes `val` to the `avail_event` field of the used ring, using
    // the provided ordering.
    fn set_avail_event<M: GuestMemory>(
        &self,
        mem: &M,
        val: u16,
        order: Ordering,
    ) -> Result<(), Error> {
        // This can not overflow an u64 since it is working with relatively small numbers compared
        // to u64::MAX.
        let avail_event_offset =
            VIRTQ_USED_RING_HEADER_SIZE + VIRTQ_USED_ELEMENT_SIZE * u64::from(self.size);
        let addr = self
            .used_ring
            .checked_add(avail_event_offset)
            .ok_or(Error::AddressOverflow)?;

        mem.store(u16::to_le(val), addr, order)
            .map_err(Error::GuestMemory)
    }

    // Set the value of the `flags` field of the used ring, applying the specified ordering.
    fn set_used_flags<M: GuestMemory>(
        &mut self,
        mem: &M,
        val: u16,
        order: Ordering,
    ) -> Result<(), Error> {
        mem.store(u16::to_le(val), self.used_ring, order)
            .map_err(Error::GuestMemory)
    }

    // Write the appropriate values to enable or disable notifications from the driver.
    //
    // Every access in this method uses `Relaxed` ordering because a fence is added by the caller
    // when appropriate.
    fn set_notification<M: GuestMemory>(&mut self, mem: &M, enable: bool) -> Result<(), Error> {
        if enable {
            if self.event_idx_enabled {
                // We call `set_avail_event` using the `next_avail` value, instead of reading
                // and using the current `avail_idx` to avoid missing notifications. More
                // details in `enable_notification`.
                self.set_avail_event(mem, self.next_avail.0, Ordering::Relaxed)
            } else {
                self.set_used_flags(mem, 0, Ordering::Relaxed)
            }
        } else if !self.event_idx_enabled {
            self.set_used_flags(mem, VRING_USED_F_NO_NOTIFY as u16, Ordering::Relaxed)
        } else {
            // Notifications are effectively disabled by default after triggering once when
            // `VIRTIO_F_EVENT_IDX` is negotiated, so we don't do anything in that case.
            Ok(())
        }
    }

    // Return the value present in the used_event field of the avail ring.
    //
    // If the VIRTIO_F_EVENT_IDX feature bit is not negotiated, the flags field in the available
    // ring offers a crude mechanism for the driver to inform the device that it doesn’t want
    // interrupts when buffers are used. Otherwise virtq_avail.used_event is a more performant
    // alternative where the driver specifies how far the device can progress before interrupting.
    //
    // Neither of these interrupt suppression methods are reliable, as they are not synchronized
    // with the device, but they serve as useful optimizations. So we only ensure access to the
    // virtq_avail.used_event is atomic, but do not need to synchronize with other memory accesses.
    fn used_event<M: GuestMemory>(&self, mem: &M, order: Ordering) -> Result<Wrapping<u16>, Error> {
        // This can not overflow an u64 since it is working with relatively small numbers compared
        // to u64::MAX.
        let used_event_offset =
            VIRTQ_AVAIL_RING_HEADER_SIZE + u64::from(self.size) * VIRTQ_AVAIL_ELEMENT_SIZE;
        let used_event_addr = self
            .avail_ring
            .checked_add(used_event_offset)
            .ok_or(Error::AddressOverflow)?;

        mem.load(used_event_addr, order)
            .map(u16::from_le)
            .map(Wrapping)
            .map_err(Error::GuestMemory)
    }
}

impl<'a> QueueStateGuard<'a> for Queue {
    type G = &'a mut Self;
}

impl QueueStateT for Queue {
    fn new(max_size: u16) -> Self {
        Queue {
            max_size,
            size: max_size,
            ready: false,
            desc_table: GuestAddress(DEFAULT_DESC_TABLE_ADDR),
            avail_ring: GuestAddress(DEFAULT_AVAIL_RING_ADDR),
            used_ring: GuestAddress(DEFAULT_USED_RING_ADDR),
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
            event_idx_enabled: false,
            num_added: Wrapping(0),
        }
    }

    fn is_valid<M: GuestMemory>(&self, mem: &M) -> bool {
        let queue_size = self.size as u64;
        let desc_table = self.desc_table;
        // The multiplication can not overflow an u64 since we are multiplying an u16 with a
        // small number.
        let desc_table_size = size_of::<Descriptor>() as u64 * queue_size;
        let avail_ring = self.avail_ring;
        // The operations below can not overflow an u64 since they're working with relatively small
        // numbers compared to u64::MAX.
        let avail_ring_size = VIRTQ_AVAIL_RING_META_SIZE + VIRTQ_AVAIL_ELEMENT_SIZE * queue_size;
        let used_ring = self.used_ring;
        let used_ring_size = VIRTQ_USED_RING_META_SIZE + VIRTQ_USED_ELEMENT_SIZE * queue_size;

        if !self.ready {
            error!("attempt to use virtio queue that is not marked ready");
            false
        } else if desc_table
            .checked_add(desc_table_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue descriptor table goes out of bounds: start:0x{:08x} size:0x{:08x}",
                desc_table.raw_value(),
                desc_table_size
            );
            false
        } else if avail_ring
            .checked_add(avail_ring_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue available ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                avail_ring.raw_value(),
                avail_ring_size
            );
            false
        } else if used_ring
            .checked_add(used_ring_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue used ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                used_ring.raw_value(),
                used_ring_size
            );
            false
        } else {
            true
        }
    }

    fn reset(&mut self) {
        self.ready = false;
        self.size = self.max_size;
        self.desc_table = GuestAddress(DEFAULT_DESC_TABLE_ADDR);
        self.avail_ring = GuestAddress(DEFAULT_AVAIL_RING_ADDR);
        self.used_ring = GuestAddress(DEFAULT_USED_RING_ADDR);
        self.next_avail = Wrapping(0);
        self.next_used = Wrapping(0);
        self.num_added = Wrapping(0);
        self.event_idx_enabled = false;
    }

    fn lock(&mut self) -> <Self as QueueStateGuard>::G {
        self
    }

    fn max_size(&self) -> u16 {
        self.max_size
    }

    fn size(&self) -> u16 {
        self.size
    }

    fn set_size(&mut self, size: u16) {
        if size > self.max_size() || size == 0 || (size & (size - 1)) != 0 {
            error!("virtio queue with invalid size: {}", size);
            return;
        }
        self.size = size;
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    fn set_desc_table_address(&mut self, low: Option<u32>, high: Option<u32>) {
        let low = low.unwrap_or(self.desc_table.0 as u32) as u64;
        let high = high.unwrap_or((self.desc_table.0 >> 32) as u32) as u64;

        let desc_table = GuestAddress((high << 32) | low);
        if desc_table.mask(0xf) != 0 {
            error!("virtio queue descriptor table breaks alignment constraints");
            return;
        }
        self.desc_table = desc_table;
    }

    fn set_avail_ring_address(&mut self, low: Option<u32>, high: Option<u32>) {
        let low = low.unwrap_or(self.avail_ring.0 as u32) as u64;
        let high = high.unwrap_or((self.avail_ring.0 >> 32) as u32) as u64;

        let avail_ring = GuestAddress((high << 32) | low);
        if avail_ring.mask(0x1) != 0 {
            error!("virtio queue available ring breaks alignment constraints");
            return;
        }
        self.avail_ring = avail_ring;
    }

    fn set_used_ring_address(&mut self, low: Option<u32>, high: Option<u32>) {
        let low = low.unwrap_or(self.used_ring.0 as u32) as u64;
        let high = high.unwrap_or((self.used_ring.0 >> 32) as u32) as u64;

        let used_ring = GuestAddress((high << 32) | low);
        if used_ring.mask(0x3) != 0 {
            error!("virtio queue used ring breaks alignment constraints");
            return;
        }
        self.used_ring = used_ring;
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx_enabled = enabled;
    }

    fn avail_idx<M>(&self, mem: &M, order: Ordering) -> Result<Wrapping<u16>, Error>
    where
        M: GuestMemory + ?Sized,
    {
        let addr = self
            .avail_ring
            .checked_add(2)
            .ok_or(Error::AddressOverflow)?;

        mem.load(addr, order)
            .map(u16::from_le)
            .map(Wrapping)
            .map_err(Error::GuestMemory)
    }

    fn used_idx<M: GuestMemory>(&self, mem: &M, order: Ordering) -> Result<Wrapping<u16>, Error> {
        let addr = self
            .used_ring
            .checked_add(2)
            .ok_or(Error::AddressOverflow)?;

        mem.load(addr, order)
            .map(u16::from_le)
            .map(Wrapping)
            .map_err(Error::GuestMemory)
    }

    fn add_used<M: GuestMemory>(
        &mut self,
        mem: &M,
        head_index: u16,
        len: u32,
    ) -> Result<(), Error> {
        if head_index >= self.size {
            error!(
                "attempted to add out of bounds descriptor to used ring: {}",
                head_index
            );
            return Err(Error::InvalidDescriptorIndex);
        }

        let next_used_index = u64::from(self.next_used.0 % self.size);
        // This can not overflow an u64 since it is working with relatively small numbers compared
        // to u64::MAX.
        let offset = VIRTQ_USED_RING_HEADER_SIZE + next_used_index * VIRTQ_USED_ELEMENT_SIZE;
        let addr = self
            .used_ring
            .checked_add(offset)
            .ok_or(Error::AddressOverflow)?;
        mem.write_obj(VirtqUsedElem::new(head_index.into(), len), addr)
            .map_err(Error::GuestMemory)?;

        self.next_used += Wrapping(1);
        self.num_added += Wrapping(1);

        mem.store(
            u16::to_le(self.next_used.0),
            self.used_ring
                .checked_add(2)
                .ok_or(Error::AddressOverflow)?,
            Ordering::Release,
        )
        .map_err(Error::GuestMemory)
    }

    // TODO: Turn this into a doc comment/example.
    // With the current implementation, a common way of consuming entries from the available ring
    // while also leveraging notification suppression is to use a loop, for example:
    //
    // loop {
    //     // We have to explicitly disable notifications if `VIRTIO_F_EVENT_IDX` has not been
    //     // negotiated.
    //     self.disable_notification()?;
    //
    //     for chain in self.iter()? {
    //         // Do something with each chain ...
    //         // Let's assume we process all available chains here.
    //     }
    //
    //     // If `enable_notification` returns `true`, the driver has added more entries to the
    //     // available ring.
    //     if !self.enable_notification()? {
    //         break;
    //     }
    // }
    fn enable_notification<M: GuestMemory>(&mut self, mem: &M) -> Result<bool, Error> {
        self.set_notification(mem, true)?;
        // Ensures the following read is not reordered before any previous write operation.
        fence(Ordering::SeqCst);

        // We double check here to avoid the situation where the available ring has been updated
        // just before we re-enabled notifications, and it's possible to miss one. We compare the
        // current `avail_idx` value to `self.next_avail` because it's where we stopped processing
        // entries. There are situations where we intentionally avoid processing everything in the
        // available ring (which will cause this method to return `true`), but in that case we'll
        // probably not re-enable notifications as we already know there are pending entries.
        self.avail_idx(mem, Ordering::Relaxed)
            .map(|idx| idx != self.next_avail)
    }

    fn disable_notification<M: GuestMemory>(&mut self, mem: &M) -> Result<(), Error> {
        self.set_notification(mem, false)
    }

    fn needs_notification<M: GuestMemory>(&mut self, mem: &M) -> Result<bool, Error> {
        let used_idx = self.next_used;

        // Complete all the writes in add_used() before reading the event.
        fence(Ordering::SeqCst);

        // The VRING_AVAIL_F_NO_INTERRUPT flag isn't supported yet.

        // When the `EVENT_IDX` feature is negotiated, the driver writes into `used_event`
        // a value that's used by the device to determine whether a notification must
        // be submitted after adding a descriptor chain to the used ring. According to the
        // standard, the notification must be sent when `next_used == used_event + 1`, but
        // various device model implementations rely on an inequality instead, most likely
        // to also support use cases where a bunch of descriptor chains are added to the used
        // ring first, and only afterwards the `needs_notification` logic is called. For example,
        // the approach based on `num_added` below is taken from the Linux Kernel implementation
        // (i.e. https://elixir.bootlin.com/linux/v5.15.35/source/drivers/virtio/virtio_ring.c#L661)

        // The `old` variable below is used to determine the value of `next_used` from when
        // `needs_notification` was called last (each `needs_notification` call resets `num_added`
        // to zero, while each `add_used` called increments it by one). Then, the logic below
        // uses wrapped arithmetic to see whether `used_event` can be found between `old` and
        // `next_used` in the circular sequence space of the used ring.
        if self.event_idx_enabled {
            let used_event = self.used_event(mem, Ordering::Relaxed)?;
            let old = used_idx - self.num_added;
            self.num_added = Wrapping(0);

            return Ok(used_idx - used_event - Wrapping(1) < used_idx - old);
        }

        Ok(true)
    }

    fn next_avail(&self) -> u16 {
        self.next_avail.0
    }

    fn set_next_avail(&mut self, next_avail: u16) {
        self.next_avail = Wrapping(next_avail);
    }

    fn next_used(&self) -> u16 {
        self.next_used.0
    }

    fn set_next_used(&mut self, next_used: u16) {
        self.next_used = Wrapping(next_used);
    }

    fn pop_descriptor_chain<M>(&mut self, mem: M) -> Option<DescriptorChain<M>>
    where
        M: Clone + Deref,
        M::Target: GuestMemory,
    {
        // Default, iter-based impl. Will be subsequently improved.
        match self.iter(mem) {
            Ok(mut iter) => iter.next(),
            Err(e) => {
                error!("Iterator error {}", e);
                None
            }
        }
    }
}

impl QueueStateOwnedT for Queue {
    fn iter<M>(&mut self, mem: M) -> Result<AvailIter<'_, M>, Error>
    where
        M: Deref,
        M::Target: GuestMemory,
    {
        self.avail_idx(mem.deref(), Ordering::Acquire)
            .map(move |idx| AvailIter::new(mem, idx, self))
    }

    fn go_to_previous_position(&mut self) {
        self.next_avail -= Wrapping(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defs::{DEFAULT_AVAIL_RING_ADDR, DEFAULT_DESC_TABLE_ADDR, DEFAULT_USED_RING_ADDR};
    use crate::mock::MockSplitQueue;
    use crate::Descriptor;
    use virtio_bindings::bindings::virtio_ring::{
        VRING_DESC_F_NEXT, VRING_DESC_F_WRITE, VRING_USED_F_NO_NOTIFY,
    };

    use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

    #[test]
    fn test_queue_is_valid() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(m, 16);
        let mut q: Queue = vq.create_queue();

        // q is currently valid
        assert!(q.is_valid(m));

        // shouldn't be valid when not marked as ready
        q.set_ready(false);
        assert!(!q.ready());
        assert!(!q.is_valid(m));
        q.set_ready(true);

        // shouldn't be allowed to set a size > max_size
        q.set_size(q.max_size() << 1);
        assert_eq!(q.size, q.max_size());

        // or set the size to 0
        q.set_size(0);
        assert_eq!(q.size, q.max_size());

        // or set a size which is not a power of 2
        q.set_size(11);
        assert_eq!(q.size, q.max_size());

        // but should be allowed to set a size if 0 < size <= max_size and size is a power of two
        q.set_size(4);
        assert_eq!(q.size, 4);
        q.size = q.max_size();

        // shouldn't be allowed to set an address that breaks the alignment constraint
        q.set_desc_table_address(Some(0xf), None);
        assert_eq!(q.desc_table.0, vq.desc_table_addr().0);
        // should be allowed to set an aligned out of bounds address
        q.set_desc_table_address(Some(0xffff_fff0), None);
        assert_eq!(q.desc_table.0, 0xffff_fff0);
        // but shouldn't be valid
        assert!(!q.is_valid(m));
        // but should be allowed to set a valid description table address
        q.set_desc_table_address(Some(0x10), None);
        assert_eq!(q.desc_table.0, 0x10);
        assert!(q.is_valid(m));
        let addr = vq.desc_table_addr().0;
        q.set_desc_table_address(Some(addr as u32), Some((addr >> 32) as u32));

        // shouldn't be allowed to set an address that breaks the alignment constraint
        q.set_avail_ring_address(Some(0x1), None);
        assert_eq!(q.avail_ring.0, vq.avail_addr().0);
        // should be allowed to set an aligned out of bounds address
        q.set_avail_ring_address(Some(0xffff_fffe), None);
        assert_eq!(q.avail_ring.0, 0xffff_fffe);
        // but shouldn't be valid
        assert!(!q.is_valid(m));
        // but should be allowed to set a valid available ring address
        q.set_avail_ring_address(Some(0x2), None);
        assert_eq!(q.avail_ring.0, 0x2);
        assert!(q.is_valid(m));
        let addr = vq.avail_addr().0;
        q.set_avail_ring_address(Some(addr as u32), Some((addr >> 32) as u32));

        // shouldn't be allowed to set an address that breaks the alignment constraint
        q.set_used_ring_address(Some(0x3), None);
        assert_eq!(q.used_ring.0, vq.used_addr().0);
        // should be allowed to set an aligned out of bounds address
        q.set_used_ring_address(Some(0xffff_fffc), None);
        assert_eq!(q.used_ring.0, 0xffff_fffc);
        // but shouldn't be valid
        assert!(!q.is_valid(m));
        // but should be allowed to set a valid used ring address
        q.set_used_ring_address(Some(0x4), None);
        assert_eq!(q.used_ring.0, 0x4);
        let addr = vq.used_addr().0;
        q.set_used_ring_address(Some(addr as u32), Some((addr >> 32) as u32));
        assert!(q.is_valid(m));
    }

    #[test]
    fn test_add_used() {
        let mem = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(mem, 16);
        let mut q: Queue = vq.create_queue();

        assert_eq!(q.used_idx(mem, Ordering::Acquire).unwrap(), Wrapping(0));
        assert_eq!(u16::from_le(vq.used().idx().load()), 0);

        // index too large
        assert!(q.add_used(mem, 16, 0x1000).is_err());
        assert_eq!(u16::from_le(vq.used().idx().load()), 0);

        // should be ok
        q.add_used(mem, 1, 0x1000).unwrap();
        assert_eq!(q.next_used, Wrapping(1));
        assert_eq!(q.used_idx(mem, Ordering::Acquire).unwrap(), Wrapping(1));
        assert_eq!(u16::from_le(vq.used().idx().load()), 1);

        let x = vq.used().ring().ref_at(0).unwrap().load();
        assert_eq!(x.id(), 1);
        assert_eq!(x.len(), 0x1000);
    }

    #[test]
    fn test_reset_queue() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(m, 16);
        let mut q: Queue = vq.create_queue();

        q.set_size(8);
        // The address set by `MockSplitQueue` for the descriptor table is DEFAULT_DESC_TABLE_ADDR,
        // so let's change it for testing the reset.
        q.set_desc_table_address(Some(0x5000), None);
        // Same for `event_idx_enabled`, `next_avail` `next_used` and `signalled_used`.
        q.set_event_idx(true);
        q.set_next_avail(2);
        q.set_next_used(4);
        q.num_added = Wrapping(15);
        assert_eq!(q.size, 8);
        // `create_queue` also marks the queue as ready.
        assert!(q.ready);
        assert_ne!(q.desc_table, GuestAddress(DEFAULT_DESC_TABLE_ADDR));
        assert_ne!(q.avail_ring, GuestAddress(DEFAULT_AVAIL_RING_ADDR));
        assert_ne!(q.used_ring, GuestAddress(DEFAULT_USED_RING_ADDR));
        assert_ne!(q.next_avail, Wrapping(0));
        assert_ne!(q.next_used, Wrapping(0));
        assert_ne!(q.num_added, Wrapping(0));
        assert!(q.event_idx_enabled);

        q.reset();
        assert_eq!(q.size, 16);
        assert!(!q.ready);
        assert_eq!(q.desc_table, GuestAddress(DEFAULT_DESC_TABLE_ADDR));
        assert_eq!(q.avail_ring, GuestAddress(DEFAULT_AVAIL_RING_ADDR));
        assert_eq!(q.used_ring, GuestAddress(DEFAULT_USED_RING_ADDR));
        assert_eq!(q.next_avail, Wrapping(0));
        assert_eq!(q.next_used, Wrapping(0));
        assert_eq!(q.num_added, Wrapping(0));
        assert!(!q.event_idx_enabled);
    }

    #[test]
    fn test_needs_notification() {
        let mem = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let qsize = 16;
        let vq = MockSplitQueue::new(mem, qsize);
        let mut q: Queue = vq.create_queue();
        let avail_addr = vq.avail_addr();

        // It should always return true when EVENT_IDX isn't enabled.
        for i in 0..qsize {
            q.next_used = Wrapping(i);
            assert!(q.needs_notification(mem).unwrap());
        }

        mem.write_obj::<u16>(
            u16::to_le(4),
            avail_addr.unchecked_add(4 + qsize as u64 * 2),
        )
        .unwrap();
        q.set_event_idx(true);

        // Incrementing up to this value causes an `u16` to wrap back to 0.
        let wrap = u32::from(u16::MAX) + 1;

        for i in 0..wrap + 12 {
            q.next_used = Wrapping(i as u16);
            // Let's test wrapping around the maximum index value as well.
            // `num_added` needs to be at least `1` to represent the fact that new descriptor
            // chains have be added to the used ring since the last time `needs_notification`
            // returned.
            q.num_added = Wrapping(1);
            let expected = i == 5 || i == (5 + wrap);
            assert_eq!((q.needs_notification(mem).unwrap(), i), (expected, i));
        }

        mem.write_obj::<u16>(
            u16::to_le(8),
            avail_addr.unchecked_add(4 + qsize as u64 * 2),
        )
        .unwrap();

        // Returns `false` because the current `used_event` value is behind both `next_used` and
        // the value of `next_used` at the time when `needs_notification` last returned (which is
        // computed based on `num_added` as described in the comments for `needs_notification`.
        assert!(!q.needs_notification(mem).unwrap());

        mem.write_obj::<u16>(
            u16::to_le(15),
            avail_addr.unchecked_add(4 + qsize as u64 * 2),
        )
        .unwrap();

        q.num_added = Wrapping(1);
        assert!(!q.needs_notification(mem).unwrap());

        q.next_used = Wrapping(15);
        q.num_added = Wrapping(1);
        assert!(!q.needs_notification(mem).unwrap());

        q.next_used = Wrapping(16);
        q.num_added = Wrapping(1);
        assert!(q.needs_notification(mem).unwrap());

        // Calling `needs_notification` again immediately returns `false`.
        assert!(!q.needs_notification(mem).unwrap());

        mem.write_obj::<u16>(
            u16::to_le(u16::MAX - 3),
            avail_addr.unchecked_add(4 + qsize as u64 * 2),
        )
        .unwrap();
        q.next_used = Wrapping(u16::MAX - 2);
        q.num_added = Wrapping(1);
        // Returns `true` because, when looking at circular sequence of indices of the used ring,
        // the value we wrote in the `used_event` appears between the "old" value of `next_used`
        // (i.e. `next_used` - `num_added`) and the current `next_used`, thus suggesting that we
        // need to notify the driver.
        assert!(q.needs_notification(mem).unwrap());
    }

    #[test]
    fn test_enable_disable_notification() {
        let mem = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(mem, 16);

        let mut q: Queue = vq.create_queue();
        let used_addr = vq.used_addr();

        assert!(!q.event_idx_enabled);

        q.enable_notification(mem).unwrap();
        let v = mem.read_obj::<u16>(used_addr).map(u16::from_le).unwrap();
        assert_eq!(v, 0);

        q.disable_notification(mem).unwrap();
        let v = mem.read_obj::<u16>(used_addr).map(u16::from_le).unwrap();
        assert_eq!(v, VRING_USED_F_NO_NOTIFY as u16);

        q.enable_notification(mem).unwrap();
        let v = mem.read_obj::<u16>(used_addr).map(u16::from_le).unwrap();
        assert_eq!(v, 0);

        q.set_event_idx(true);
        let avail_addr = vq.avail_addr();
        mem.write_obj::<u16>(u16::to_le(2), avail_addr.unchecked_add(2))
            .unwrap();

        assert!(q.enable_notification(mem).unwrap());
        q.next_avail = Wrapping(2);
        assert!(!q.enable_notification(mem).unwrap());

        mem.write_obj::<u16>(u16::to_le(8), avail_addr.unchecked_add(2))
            .unwrap();

        assert!(q.enable_notification(mem).unwrap());
        q.next_avail = Wrapping(8);
        assert!(!q.enable_notification(mem).unwrap());
    }

    #[test]
    fn test_consume_chains_with_notif() {
        let mem = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(mem, 16);

        let mut q: Queue = vq.create_queue();

        // q is currently valid.
        assert!(q.is_valid(mem));

        // The chains are (0, 1), (2, 3, 4), (5, 6), (7, 8), (9, 10, 11, 12).
        for i in 0..13 {
            let flags = match i {
                1 | 4 | 6 | 8 | 12 => 0,
                _ => VRING_DESC_F_NEXT,
            };

            let desc = Descriptor::new((0x1000 * (i + 1)) as u64, 0x1000, flags as u16, i + 1);
            vq.desc_table().store(i, desc).unwrap();
        }

        vq.avail().ring().ref_at(0).unwrap().store(u16::to_le(0));
        vq.avail().ring().ref_at(1).unwrap().store(u16::to_le(2));
        vq.avail().ring().ref_at(2).unwrap().store(u16::to_le(5));
        vq.avail().ring().ref_at(3).unwrap().store(u16::to_le(7));
        vq.avail().ring().ref_at(4).unwrap().store(u16::to_le(9));
        // Let the device know it can consume chains with the index < 2.
        vq.avail().idx().store(u16::to_le(2));
        // No descriptor chains are consumed at this point.
        assert_eq!(q.next_avail(), 0);

        let mut i = 0;

        loop {
            i += 1;
            q.disable_notification(mem).unwrap();

            while let Some(chain) = q.iter(mem).unwrap().next() {
                // Process the descriptor chain, and then add entries to the
                // used ring.
                let head_index = chain.head_index();
                let mut desc_len = 0;
                chain.for_each(|d| {
                    if d.flags() as u32 & VRING_DESC_F_WRITE == VRING_DESC_F_WRITE {
                        desc_len += d.len();
                    }
                });
                q.add_used(mem, head_index, desc_len).unwrap();
            }
            if !q.enable_notification(mem).unwrap() {
                break;
            }
        }
        // The chains should be consumed in a single loop iteration because there's nothing updating
        // the `idx` field of the available ring in the meantime.
        assert_eq!(i, 1);
        // The next chain that can be consumed should have index 2.
        assert_eq!(q.next_avail(), 2);
        assert_eq!(q.next_used(), 2);
        // Let the device know it can consume one more chain.
        vq.avail().idx().store(u16::to_le(3));
        i = 0;

        loop {
            i += 1;
            q.disable_notification(mem).unwrap();

            while let Some(chain) = q.iter(mem).unwrap().next() {
                // Process the descriptor chain, and then add entries to the
                // used ring.
                let head_index = chain.head_index();
                let mut desc_len = 0;
                chain.for_each(|d| {
                    if d.flags() as u32 & VRING_DESC_F_WRITE == VRING_DESC_F_WRITE {
                        desc_len += d.len();
                    }
                });
                q.add_used(mem, head_index, desc_len).unwrap();
            }

            // For the simplicity of the test we are updating here the `idx` value of the available
            // ring. Ideally this should be done on a separate thread.
            // Because of this update, the loop should be iterated again to consume the new
            // available descriptor chains.
            vq.avail().idx().store(u16::to_le(4));
            if !q.enable_notification(mem).unwrap() {
                break;
            }
        }
        assert_eq!(i, 2);
        // The next chain that can be consumed should have index 4.
        assert_eq!(q.next_avail(), 4);
        assert_eq!(q.next_used(), 4);

        // Set an `idx` that is bigger than the number of entries added in the ring.
        // This is an allowed scenario, but the indexes of the chain will have unexpected values.
        vq.avail().idx().store(u16::to_le(7));
        loop {
            q.disable_notification(mem).unwrap();

            while let Some(chain) = q.iter(mem).unwrap().next() {
                // Process the descriptor chain, and then add entries to the
                // used ring.
                let head_index = chain.head_index();
                let mut desc_len = 0;
                chain.for_each(|d| {
                    if d.flags() as u32 & VRING_DESC_F_WRITE == VRING_DESC_F_WRITE {
                        desc_len += d.len();
                    }
                });
                q.add_used(mem, head_index, desc_len).unwrap();
            }
            if !q.enable_notification(mem).unwrap() {
                break;
            }
        }
        assert_eq!(q.next_avail(), 7);
        assert_eq!(q.next_used(), 7);
    }

    #[test]
    fn test_invalid_avail_idx() {
        // This is a negative test for the following MUST from the spec: `A driver MUST NOT
        // decrement the available idx on a virtqueue (ie. there is no way to “unexpose” buffers).`.
        // We validate that for this misconfiguration, the device does not panic.
        let mem = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(mem, 16);

        let mut q: Queue = vq.create_queue();

        // q is currently valid.
        assert!(q.is_valid(mem));

        // The chains are (0, 1), (2, 3, 4), (5, 6).
        for i in 0..7 {
            let flags = match i {
                1 | 4 | 6 => 0,
                _ => VRING_DESC_F_NEXT,
            };

            let desc = Descriptor::new((0x1000 * (i + 1)) as u64, 0x1000, flags as u16, i + 1);
            vq.desc_table().store(i, desc).unwrap();
        }

        vq.avail().ring().ref_at(0).unwrap().store(u16::to_le(0));
        vq.avail().ring().ref_at(1).unwrap().store(u16::to_le(2));
        vq.avail().ring().ref_at(2).unwrap().store(u16::to_le(5));
        // Let the device know it can consume chains with the index < 2.
        vq.avail().idx().store(u16::to_le(3));
        // No descriptor chains are consumed at this point.
        assert_eq!(q.next_avail(), 0);
        assert_eq!(q.next_used(), 0);

        loop {
            q.disable_notification(mem).unwrap();

            while let Some(chain) = q.iter(mem).unwrap().next() {
                // Process the descriptor chain, and then add entries to the
                // used ring.
                let head_index = chain.head_index();
                let mut desc_len = 0;
                chain.for_each(|d| {
                    if d.flags() as u32 & VRING_DESC_F_WRITE == VRING_DESC_F_WRITE {
                        desc_len += d.len();
                    }
                });
                q.add_used(mem, head_index, desc_len).unwrap();
            }
            if !q.enable_notification(mem).unwrap() {
                break;
            }
        }
        // The next chain that can be consumed should have index 3.
        assert_eq!(q.next_avail(), 3);
        assert_eq!(q.avail_idx(mem, Ordering::Acquire).unwrap(), Wrapping(3));
        assert_eq!(q.next_used(), 3);
        assert_eq!(q.used_idx(mem, Ordering::Acquire).unwrap(), Wrapping(3));
        assert!(q.lock().ready());

        // Decrement `idx` which should be forbidden. We don't enforce this thing, but we should
        // test that we don't panic in case the driver decrements it.
        vq.avail().idx().store(u16::to_le(1));

        loop {
            q.disable_notification(mem).unwrap();

            while let Some(_chain) = q.iter(mem).unwrap().next() {
                // In a real use case, we would do something with the chain here.
            }

            if !q.enable_notification(mem).unwrap() {
                break;
            }
        }
    }
}

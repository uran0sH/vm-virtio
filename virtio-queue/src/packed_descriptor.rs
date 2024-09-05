use vm_memory::{ByteValued, GuestAddress, Le16, Le32, Le64};

use virtio_bindings::bindings::virtio_ring::{
    VRING_DESC_F_INDIRECT, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE,
};

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct Descriptor {
    /// Guest physical address of device specific data.
    addr: Le64,

    /// Length of device specific data.
    len: Le32,

    /// Index of descriptor in the descriptor table.
    id: Le16,

    /// Includes next, write, and indirect bits.
    flags: Le16,
}

impl Descriptor {
    /// Return the guest physical address of the descriptor buffer.
    pub fn addr(&self) -> GuestAddress {
        GuestAddress(self.addr.into())
    }

    /// Return the length of the descriptor buffer.
    pub fn len(&self) -> u32 {
        self.len.into()
    }

    /// Return the flags for this descriptor, including next, write and indirect bits.
    pub fn flags(&self) -> u16 {
        self.flags.into()
    }

    /// Return the index of the descriptor in the descriptor table.
    pub fn id(&self) -> u16 {
        self.id.into()
    }

    /// Check whether this descriptor refers to a buffer containing an indirect descriptor table.
    pub fn refers_to_indirect_table(&self) -> bool {
        self.flags() & VRING_DESC_F_INDIRECT as u16 != 0
    }

    /// Check whether the `VIRTQ_DESC_F_NEXT` is set for the descriptor.
    pub fn has_next(&self) -> bool {
        self.flags() & VRING_DESC_F_NEXT as u16 != 0
    }

    /// Check if the driver designated this as a write only descriptor.
    ///
    /// If this is false, this descriptor is read only.
    /// Write only means the the emulated device can write and the driver can read.
    pub fn is_write_only(&self) -> bool {
        self.flags() & VRING_DESC_F_WRITE as u16 != 0
    }
}

impl Descriptor {
    /// Create a new descriptor.
    ///
    /// # Arguments
    /// * `addr` - the guest physical address of the descriptor buffer.
    /// * `len` - the length of the descriptor buffer.
    /// * `flags` - the `flags` for the descriptor.
    /// * `next` - the `next` field of the descriptor.
    pub fn new(addr: u64, len: u32, id: u16, flags: u16) -> Self {
        Descriptor {
            addr: addr.into(),
            len: len.into(),
            id: id.into(),
            flags: flags.into(),
        }
    }

    /// Set the guest physical address of the descriptor buffer.
    pub fn set_addr(&mut self, addr: u64) {
        self.addr = addr.into();
    }

    /// Set the length of the descriptor buffer.
    pub fn set_len(&mut self, len: u32) {
        self.len = len.into();
    }

    /// Set the flags for this descriptor.
    pub fn set_flags(&mut self, flags: u16) {
        self.flags = flags.into();
    }

    /// Set the value stored in the `next` field of the descriptor.
    pub fn set_id(&mut self, id: u16) {
        self.id = id.into();
    }
}

unsafe impl ByteValued for Descriptor {}

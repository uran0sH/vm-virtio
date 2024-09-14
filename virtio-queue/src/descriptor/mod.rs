// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// Copyright © 2019 Intel Corporation
//
// Copyright (C) 2020-2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause
pub mod packed;
pub mod split;

use vm_memory::{ByteValued, GuestAddress};

use virtio_bindings::bindings::virtio_ring::{
    VRING_DESC_F_INDIRECT, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE,
};

pub use packed::Descriptor as PackedDescriptor;
pub use split::Descriptor as SplitDescriptor;
// pub use packed_descriptor::PackedDescEvent;

/// A virtio descriptor constraints with C representation.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum Descriptor {
    /// the split descriptor
    SplitDescriptor(SplitDescriptor),
    /// the packed descriptor
    PackedDescriptor(PackedDescriptor),
}

unsafe impl ByteValued for Descriptor {}

#[allow(clippy::len_without_is_empty)]
impl Descriptor {
    /// Return the guest physical address of the descriptor buffer.
    pub fn addr(&self) -> GuestAddress {
        match self {
            Descriptor::SplitDescriptor(desc) => desc.addr(),
            Descriptor::PackedDescriptor(desc) => desc.addr(),
        }
    }

    /// Return the length of the descriptor buffer.
    pub fn len(&self) -> u32 {
        match self {
            Descriptor::SplitDescriptor(desc) => desc.len(),
            Descriptor::PackedDescriptor(desc) => desc.len(),
        }
    }

    /// Return the flags for this descriptor, including next, write and indirect bits.
    pub fn flags(&self) -> u16 {
        match self {
            Descriptor::SplitDescriptor(desc) => desc.flags(),
            Descriptor::PackedDescriptor(desc) => desc.flags(),
        }
    }

    /// Return the value stored in the `next` field of the descriptor.
    pub fn next(&self) -> u16 {
        match self {
            Descriptor::SplitDescriptor(desc) => desc.next(),
            Descriptor::PackedDescriptor(_) => unimplemented!(),
        }
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

    /// Set the flags for this descriptor.
    ///
    /// # Arguments
    ///
    /// * `flags` - The flags to set for the descriptor.
    pub fn set_flags(&mut self, flags: u16) {
        match self {
            Descriptor::SplitDescriptor(desc) => desc.set_flags(flags),
            Descriptor::PackedDescriptor(desc) => desc.set_flags(flags),
        }
    }

    /// Set the address of the descriptor.
    pub fn set_addr(&mut self, addr: u64) {
        match self {
            Descriptor::SplitDescriptor(desc) => desc.set_addr(addr),
            Descriptor::PackedDescriptor(desc) => desc.set_addr(addr),
        }
    }

    /// Set the length of the descriptor buffer.
    pub fn set_len(&mut self, len: u32) {
        match self {
            Descriptor::SplitDescriptor(desc) => desc.set_len(len),
            Descriptor::PackedDescriptor(desc) => desc.set_len(len),
        }
    }
}

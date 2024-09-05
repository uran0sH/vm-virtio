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

use vm_memory::{ByteValued, GuestAddress, Le16, Le32, Le64};

use virtio_bindings::bindings::virtio_ring::{
    VRING_DESC_F_INDIRECT, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE,
};

use crate::packed_descriptor;
use crate::split_descriptor;

/// A virtio descriptor constraints with C representation.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum Descriptor {
    /// the split descriptor
    SplitDescriptor(split_descriptor::Descriptor),
    /// the packed descriptor
    PackedDescriptor(packed_descriptor::Descriptor),
}

unsafe impl ByteValued for Descriptor {}

#[allow(clippy::len_without_is_empty)]
impl Descriptor {
    /// Return the guest physical address of the descriptor buffer.
    pub fn addr(&self) -> GuestAddress {
        // GuestAddress(self.addr.into())
        match self {
            Descriptor::SplitDescriptor(desc) => desc.addr(),
            Descriptor::PackedDescriptor(desc) => desc.addr(),
        }
    }

    /// Return the length of the descriptor buffer.
    pub fn len(&self) -> u32 {
        // self.len.into()
        match self {
            Descriptor::SplitDescriptor(desc) => desc.len(),
            Descriptor::PackedDescriptor(desc) => desc.len(),
        }
    }

    /// Return the flags for this descriptor, including next, write and indirect bits.
    pub fn flags(&self) -> u16 {
        // self.flags.into()
        match self {
            Descriptor::SplitDescriptor(desc) => desc.flags(),
            Descriptor::PackedDescriptor(desc) => desc.flags(),
        }
    }

    /// Return the value stored in the `next` field of the descriptor.
    pub fn next(&self) -> u16 {
        // self.next.into()
        match self {
            Descriptor::SplitDescriptor(desc) => desc.next(),
            Descriptor::PackedDescriptor(desc) => unimplemented!(),
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
}

#[derive(Clone, Copy, Debug)]
pub struct PackedDescEvent {
    off_wrap: Le16,
    flags: Le16,
}

impl PackedDescEvent {
    pub fn set_off_wrap(&mut self, off_wrap: u16) {
        self.off_wrap = off_wrap.into();
    }

    pub fn set_flags(&mut self, flags: u16) {
        self.flags = flags.into();
    }

    pub fn get_off_wrap(&self) -> u16 {
        self.off_wrap.into()
    }

    pub fn get_flags(&self) -> u16 {
        self.flags.into()
    }
}

unsafe impl ByteValued for PackedDescEvent {}
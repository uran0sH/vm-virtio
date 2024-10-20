//! Descriptor types for virtio queue.

use vm_memory::{ByteValued, Le16, Le32, Le64};

pub mod packed;
pub mod split;

/// A virtio descriptor's layout constraints with C representation.
/// This is a unified representation of the memory layout order
/// for packed descriptors and split descriptors.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Descriptor(Le64, Le32, Le16, Le16);

// SAFETY: This is safe because `Descriptor` contains only wrappers over POD types and
// all accesses through safe `vm-memory` API will validate any garbage that could be
// included in there.
unsafe impl ByteValued for Descriptor {}

impl From<split::Descriptor> for Descriptor {
    fn from(desc: split::Descriptor) -> Self {
        Descriptor(
            Le64::from(desc.addr().0),
            Le32::from(desc.len()),
            Le16::from(desc.flags()),
            Le16::from(desc.next()),
        )
    }
}

impl From<Descriptor> for split::Descriptor {
    fn from(desc: Descriptor) -> split::Descriptor {
        split::Descriptor::new(desc.0.into(), desc.1.into(), desc.2.into(), desc.3.into())
    }
}

impl From<Descriptor> for packed::Descriptor {
    fn from(desc: Descriptor) -> packed::Descriptor {
        packed::Descriptor::new(desc.0.into(), desc.1.into(), desc.2.into(), desc.3.into())
    }
}

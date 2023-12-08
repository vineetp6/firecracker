// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::ErrorKind;

use libc::{c_void, iovec, size_t};
use vm_memory::{
    GuestMemoryError, ReadVolatile, VolatileMemoryError, VolatileSlice, WriteVolatile,
};

use crate::devices::virtio::queue::DescriptorChain;
use crate::vstate::memory::{Bitmap, GuestMemory};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum IoVecError {
    /// Tried to create an `IoVec` from a write-only descriptor chain
    WriteOnlyDescriptor,
    /// Tried to create an 'IoVecMut` from a read-only descriptor chain
    ReadOnlyDescriptor,
    /// Guest memory error: {0}
    GuestMemory(#[from] GuestMemoryError),
}

/// This is essentially a wrapper of a `Vec<libc::iovec>` which can be passed to `libc::writev`.
///
/// It describes a buffer passed to us by the guest that is scattered across multiple
/// memory regions. Additionally, this wrapper provides methods that allow reading arbitrary ranges
/// of data from that buffer.
#[derive(Debug)]
pub struct IoVecBuffer {
    // container of the memory regions included in this IO vector
    vecs: Vec<iovec>,
    // Total length of the IoVecBuffer
    len: usize,
}

impl IoVecBuffer {
    /// Create an `IoVecBuffer` from a `DescriptorChain`
    pub fn from_descriptor_chain(head: DescriptorChain) -> Result<Self, IoVecError> {
        let mut vecs = vec![];
        let mut len = 0usize;

        let mut next_descriptor = Some(head);
        while let Some(desc) = next_descriptor {
            if desc.is_write_only() {
                return Err(IoVecError::WriteOnlyDescriptor);
            }

            // We use get_slice instead of `get_host_address` here in order to have the whole
            // range of the descriptor chain checked, i.e. [addr, addr + len) is a valid memory
            // region in the GuestMemoryMmap.
            let iov_base = desc
                .mem
                .get_slice(desc.addr, desc.len as usize)?
                .ptr_guard_mut()
                .as_ptr()
                .cast::<c_void>();
            vecs.push(iovec {
                iov_base,
                iov_len: desc.len as size_t,
            });
            len += desc.len as usize;

            next_descriptor = desc.next_descriptor();
        }

        Ok(Self { vecs, len })
    }

    /// Get the total length of the memory regions covered by this `IoVecBuffer`
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Returns a pointer to the memory keeping the `iovec` structs
    pub fn as_iovec_ptr(&self) -> *const iovec {
        self.vecs.as_ptr()
    }

    /// Returns the length of the `iovec` array.
    pub fn iovec_count(&self) -> usize {
        self.vecs.len()
    }

    /// Reads a number of bytes from the `IoVecBuffer` starting at a given offset.
    ///
    /// This will try to fill `buf` reading bytes from the `IoVecBuffer` starting from
    /// the given offset.
    ///
    /// # Returns
    ///
    /// The number of bytes read (if any)
    pub fn read_at(&self, mut buf: &mut [u8], offset: usize) -> Option<usize> {
        if offset < self.len() {
            // Make sure we only read up to the end of the `IoVecBuffer`.
            let size = buf.len().min(self.len() - offset);
            // write_volatile for &mut [u8] is infallible
            self.read_volatile_at(&mut buf, offset, size).ok()
        } else {
            // If `offset` is past size, there's nothing to read.
            None
        }
    }

    /// Reads up to `len` bytes from the `IoVecBuffer` starting at the given offset.
    ///
    /// This will try to write to the given [`WriteVolatile`].
    pub fn read_volatile_at<W: WriteVolatile>(
        &self,
        dst: &mut W,
        mut offset: usize,
        mut len: usize,
    ) -> Result<usize, VolatileMemoryError> {
        let mut total_bytes_read = 0;

        for iov in &self.vecs {
            if len == 0 {
                break;
            }

            if offset >= iov.iov_len {
                offset -= iov.iov_len;
                continue;
            }

            let mut slice =
                // SAFETY: the constructor IoVecBufferMut::from_descriptor_chain ensures that
                // all iovecs contained point towards valid ranges of guest memory
                unsafe { VolatileSlice::new(iov.iov_base.cast(), iov.iov_len).offset(offset)? };
            offset = 0;

            if slice.len() > len {
                slice = slice.subslice(0, len)?;
            }

            let bytes_read = loop {
                match dst.write_volatile(&slice) {
                    Err(VolatileMemoryError::IOError(err))
                        if err.kind() == ErrorKind::Interrupted =>
                    {
                        continue
                    }
                    Ok(bytes_read) => break bytes_read,
                    Err(volatile_memory_error) => return Err(volatile_memory_error),
                }
            };
            total_bytes_read += bytes_read;

            if bytes_read < slice.len() {
                break;
            }
            len -= bytes_read;
        }

        Ok(total_bytes_read)
    }
}

/// This is essentially a wrapper of a `Vec<libc::iovec>` which can be passed to `libc::readv`.
///
/// It describes a write-only buffer passed to us by the guest that is scattered across multiple
/// memory regions. Additionally, this wrapper provides methods that allow reading arbitrary ranges
/// of data from that buffer.
#[derive(Debug)]
pub struct IoVecBufferMut {
    // container of the memory regions included in this IO vector
    vecs: Vec<iovec>,
    // Total length of the IoVecBufferMut
    len: usize,
}

impl IoVecBufferMut {
    /// Create an `IoVecBufferMut` from a `DescriptorChain`
    pub fn from_descriptor_chain(head: DescriptorChain) -> Result<Self, IoVecError> {
        let mut vecs = vec![];
        let mut len = 0usize;

        for desc in head {
            if !desc.is_write_only() {
                return Err(IoVecError::ReadOnlyDescriptor);
            }

            // We use get_slice instead of `get_host_address` here in order to have the whole
            // range of the descriptor chain checked, i.e. [addr, addr + len) is a valid memory
            // region in the GuestMemoryMmap.
            let slice = desc.mem.get_slice(desc.addr, desc.len as usize)?;

            // We need to mark the area of guest memory that will be mutated through this
            // IoVecBufferMut as dirty ahead of time, as we loose access to all
            // vm-memory related information after converting down to iovecs.
            slice.bitmap().mark_dirty(0, desc.len as usize);

            let iov_base = slice.ptr_guard_mut().as_ptr().cast::<c_void>();
            vecs.push(iovec {
                iov_base,
                iov_len: desc.len as size_t,
            });
            len += desc.len as usize;
        }

        Ok(Self { vecs, len })
    }

    /// Get the total length of the memory regions covered by this `IoVecBuffer`
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Writes a number of bytes into the `IoVecBufferMut` starting at a given offset.
    ///
    /// This will try to fill `IoVecBufferMut` writing bytes from the `buf` starting from
    /// the given offset. It will write as many bytes from `buf` as they fit inside the
    /// `IoVecBufferMut` starting from `offset`.
    ///
    /// # Returns
    ///
    /// The number of bytes written (if any)
    pub fn write_at(&mut self, mut buf: &[u8], offset: usize) -> Option<usize> {
        if offset < self.len() {
            // Make sure we only write up to the end of the `IoVecBufferMut`.
            let size = buf.len().min(self.len() - offset);
            self.write_volatile_at(&mut buf, offset, size).ok()
        } else {
            // We cannot write past the end of the `IoVecBufferMut`.
            None
        }
    }

    /// Writes up to `len` bytes into the `IoVecBuffer` starting at the given offset.
    ///
    /// This will try to write to the given [`WriteVolatile`].
    pub fn write_volatile_at<W: ReadVolatile>(
        &mut self,
        src: &mut W,
        mut offset: usize,
        mut len: usize,
    ) -> Result<usize, VolatileMemoryError> {
        let mut total_bytes_read = 0;

        for iov in &self.vecs {
            if len == 0 {
                break;
            }

            if offset >= iov.iov_len {
                offset -= iov.iov_len;
                continue;
            }

            let mut slice =
                // SAFETY: the constructor IoVecBufferMut::from_descriptor_chain ensures that
                // all iovecs contained point towards valid ranges of guest memory
                unsafe { VolatileSlice::new(iov.iov_base.cast(), iov.iov_len).offset(offset)? };
            offset = 0;

            if slice.len() > len {
                slice = slice.subslice(0, len)?;
            }

            let bytes_read = loop {
                match src.read_volatile(&mut slice) {
                    Err(VolatileMemoryError::IOError(err))
                        if err.kind() == ErrorKind::Interrupted =>
                    {
                        continue
                    }
                    Ok(bytes_read) => break bytes_read,
                    Err(volatile_memory_error) => return Err(volatile_memory_error),
                }
            };
            total_bytes_read += bytes_read;

            if bytes_read < slice.len() {
                break;
            }
            len -= bytes_read;
        }

        Ok(total_bytes_read)
    }
}

#[cfg(test)]
mod tests {
    use libc::{c_void, iovec};

    use super::{IoVecBuffer, IoVecBufferMut};
    use crate::devices::virtio::queue::{Queue, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
    use crate::devices::virtio::test_utils::VirtQueue;
    use crate::vstate::memory::{Bytes, GuestAddress, GuestMemoryExtension, GuestMemoryMmap};

    impl<'a> From<&'a [u8]> for IoVecBuffer {
        fn from(buf: &'a [u8]) -> Self {
            Self {
                vecs: vec![iovec {
                    iov_base: buf.as_ptr() as *mut c_void,
                    iov_len: buf.len(),
                }],
                len: buf.len(),
            }
        }
    }

    impl<'a> From<Vec<&'a [u8]>> for IoVecBuffer {
        fn from(buffer: Vec<&'a [u8]>) -> Self {
            let mut len = 0;
            let vecs = buffer
                .into_iter()
                .map(|slice| {
                    len += slice.len();
                    iovec {
                        iov_base: slice.as_ptr() as *mut c_void,
                        iov_len: slice.len(),
                    }
                })
                .collect();

            Self { vecs, len }
        }
    }

    impl From<&mut [u8]> for IoVecBufferMut {
        fn from(buf: &mut [u8]) -> Self {
            Self {
                vecs: vec![iovec {
                    iov_base: buf.as_mut_ptr().cast::<c_void>(),
                    iov_len: buf.len(),
                }],
                len: buf.len(),
            }
        }
    }

    fn default_mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_raw_regions(
            &[
                (GuestAddress(0), 0x10000),
                (GuestAddress(0x20000), 0x10000),
                (GuestAddress(0x40000), 0x10000),
            ],
            false,
        )
        .unwrap()
    }

    fn chain(m: &GuestMemoryMmap, is_write_only: bool) -> (Queue, VirtQueue) {
        let vq = VirtQueue::new(GuestAddress(0), m, 16);

        let mut q = vq.create_queue();
        q.ready = true;

        let flags = if is_write_only {
            VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE
        } else {
            VIRTQ_DESC_F_NEXT
        };

        for j in 0..4 {
            vq.dtable[j as usize].set(0x20000 + 64 * u64::from(j), 64, flags, j + 1);
        }

        // one chain: (0, 1, 2, 3)
        vq.dtable[3].flags.set(flags & !VIRTQ_DESC_F_NEXT);
        vq.avail.ring[0].set(0);
        vq.avail.idx.set(1);

        (q, vq)
    }

    fn read_only_chain(mem: &GuestMemoryMmap) -> (Queue, VirtQueue) {
        let v: Vec<u8> = (0..=255).collect();
        mem.write_slice(&v, GuestAddress(0x20000)).unwrap();

        chain(mem, false)
    }

    fn write_only_chain(mem: &GuestMemoryMmap) -> (Queue, VirtQueue) {
        let v = vec![0; 256];
        mem.write_slice(&v, GuestAddress(0x20000)).unwrap();

        chain(mem, true)
    }

    #[test]
    fn test_access_mode() {
        let mem = default_mem();
        let (mut q, _) = read_only_chain(&mem);
        let head = q.pop(&mem).unwrap();
        assert!(IoVecBuffer::from_descriptor_chain(head).is_ok());

        let (mut q, _) = write_only_chain(&mem);
        let head = q.pop(&mem).unwrap();
        assert!(IoVecBuffer::from_descriptor_chain(head).is_err());

        let (mut q, _) = read_only_chain(&mem);
        let head = q.pop(&mem).unwrap();
        assert!(IoVecBufferMut::from_descriptor_chain(head).is_err());

        let (mut q, _) = write_only_chain(&mem);
        let head = q.pop(&mem).unwrap();
        assert!(IoVecBufferMut::from_descriptor_chain(head).is_ok());
    }

    #[test]
    fn test_iovec_length() {
        let mem = default_mem();
        let (mut q, _) = read_only_chain(&mem);
        let head = q.pop(&mem).unwrap();

        let iovec = IoVecBuffer::from_descriptor_chain(head).unwrap();
        assert_eq!(iovec.len(), 4 * 64);
    }

    #[test]
    fn test_iovec_mut_length() {
        let mem = default_mem();
        let (mut q, _) = write_only_chain(&mem);
        let head = q.pop(&mem).unwrap();

        let iovec = IoVecBufferMut::from_descriptor_chain(head).unwrap();
        assert_eq!(iovec.len(), 4 * 64);
    }

    #[test]
    fn test_iovec_read_at() {
        let mem = default_mem();
        let (mut q, _) = read_only_chain(&mem);
        let head = q.pop(&mem).unwrap();

        let iovec = IoVecBuffer::from_descriptor_chain(head).unwrap();

        let mut buf = vec![0; 257];
        assert_eq!(iovec.read_at(&mut buf[..], 0), Some(256));
        assert_eq!(buf[0..256], (0..=255).collect::<Vec<_>>());
        assert_eq!(buf[256], 0);

        let mut buf = vec![0; 5];
        assert_eq!(iovec.read_at(&mut buf[..4], 0), Some(4));
        assert_eq!(buf, vec![0u8, 1, 2, 3, 0]);

        assert_eq!(iovec.read_at(&mut buf, 0), Some(5));
        assert_eq!(buf, vec![0u8, 1, 2, 3, 4]);

        assert_eq!(iovec.read_at(&mut buf, 1), Some(5));
        assert_eq!(buf, vec![1u8, 2, 3, 4, 5]);

        assert_eq!(iovec.read_at(&mut buf, 60), Some(5));
        assert_eq!(buf, vec![60u8, 61, 62, 63, 64]);

        assert_eq!(iovec.read_at(&mut buf, 252), Some(4));
        assert_eq!(buf[0..4], vec![252u8, 253, 254, 255]);

        assert_eq!(iovec.read_at(&mut buf, 256), None);
    }

    #[test]
    fn test_iovec_mut_write_at() {
        let mem = default_mem();
        let (mut q, vq) = write_only_chain(&mem);

        // This is a descriptor chain with 4 elements 64 bytes long each.
        let head = q.pop(&mem).unwrap();

        let mut iovec = IoVecBufferMut::from_descriptor_chain(head).unwrap();
        let buf = vec![0u8, 1, 2, 3, 4];

        // One test vector for each part of the chain
        let mut test_vec1 = vec![0u8; 64];
        let mut test_vec2 = vec![0u8; 64];
        let test_vec3 = vec![0u8; 64];
        let mut test_vec4 = vec![0u8; 64];

        // Control test: Initially all three regions should be zero
        assert_eq!(iovec.write_at(&test_vec1, 0), Some(64));
        assert_eq!(iovec.write_at(&test_vec2, 64), Some(64));
        assert_eq!(iovec.write_at(&test_vec3, 128), Some(64));
        assert_eq!(iovec.write_at(&test_vec4, 192), Some(64));
        vq.dtable[0].check_data(&test_vec1);
        vq.dtable[1].check_data(&test_vec2);
        vq.dtable[2].check_data(&test_vec3);
        vq.dtable[3].check_data(&test_vec4);

        // Let's initialize test_vec1 with our buffer.
        test_vec1[..buf.len()].copy_from_slice(&buf);
        // And write just a part of it
        assert_eq!(iovec.write_at(&buf[..3], 0), Some(3));
        // Not all 5 bytes from buf should be written in memory,
        // just 3 of them.
        vq.dtable[0].check_data(&[0u8, 1, 2, 0, 0]);
        vq.dtable[1].check_data(&test_vec2);
        vq.dtable[2].check_data(&test_vec3);
        vq.dtable[3].check_data(&test_vec4);
        // But if we write the whole `buf` in memory then all
        // of it should be observable.
        assert_eq!(iovec.write_at(&buf, 0), Some(5));
        vq.dtable[0].check_data(&test_vec1);
        vq.dtable[1].check_data(&test_vec2);
        vq.dtable[2].check_data(&test_vec3);
        vq.dtable[3].check_data(&test_vec4);

        // We are now writing with an offset of 1. So, initialize
        // the corresponding part of `test_vec1`
        test_vec1[1..buf.len() + 1].copy_from_slice(&buf);
        assert_eq!(iovec.write_at(&buf, 1), Some(5));
        vq.dtable[0].check_data(&test_vec1);
        vq.dtable[1].check_data(&test_vec2);
        vq.dtable[2].check_data(&test_vec3);
        vq.dtable[3].check_data(&test_vec4);

        // Perform a write that traverses two of the underlying
        // regions. Writing at offset 60 should write 4 bytes on the
        // first region and one byte on the second
        test_vec1[60..64].copy_from_slice(&buf[0..4]);
        test_vec2[0] = 4;
        assert_eq!(iovec.write_at(&buf, 60), Some(5));
        vq.dtable[0].check_data(&test_vec1);
        vq.dtable[1].check_data(&test_vec2);
        vq.dtable[2].check_data(&test_vec3);
        vq.dtable[3].check_data(&test_vec4);

        test_vec4[63] = 3;
        test_vec4[62] = 2;
        test_vec4[61] = 1;
        // Now perform a write that does not fit in the buffer. Try writing
        // 5 bytes at offset 252 (only 4 bytes left).
        test_vec4[60..64].copy_from_slice(&buf[0..4]);
        assert_eq!(iovec.write_at(&buf, 252), Some(4));
        vq.dtable[0].check_data(&test_vec1);
        vq.dtable[1].check_data(&test_vec2);
        vq.dtable[2].check_data(&test_vec3);
        vq.dtable[3].check_data(&test_vec4);

        // Trying to add past the end of the buffer should not write anything
        assert_eq!(iovec.write_at(&buf, 256), None);
        vq.dtable[0].check_data(&test_vec1);
        vq.dtable[1].check_data(&test_vec2);
        vq.dtable[2].check_data(&test_vec3);
        vq.dtable[3].check_data(&test_vec4);
    }
}

#[cfg(kani)]
mod verification {
    use std::mem::ManuallyDrop;

    use libc::{c_void, iovec};

    use super::{IoVecBuffer, IoVecBufferMut};

    // Maximum memory size to use for our buffers. For the time being 1KB.
    const GUEST_MEMORY_SIZE: usize = 1 << 10;

    // Maximum number of descriptors in a chain to use in our proofs. The value is selected upon
    // experimenting with the execution time. Typically, in our virtio devices we use queues of up
    // to 256 entries which is the theoretical maximum length of a `DescriptorChain`, but in reality
    // our code does not make any assumption about the length of the chain, apart from it being
    // >= 1.
    const MAX_DESC_LENGTH: usize = 4;

    fn create_iovecs(mem: *mut u8, size: usize) -> (Vec<iovec>, usize) {
        let nr_descs: usize = kani::any_where(|&n| n <= MAX_DESC_LENGTH);
        let mut vecs: Vec<iovec> = Vec::with_capacity(nr_descs);
        let mut len = 0usize;
        for _ in 0..nr_descs {
            // The `IoVecBuffer(Mut)` constructors ensure that the memory region described by every
            // `Descriptor` in the chain is a valid, i.e. it is memory with then guest's memory
            // mmap. The assumption, here, that the last address is within the memory object's
            // bound substitutes these checks that `IoVecBuffer(Mut)::new() performs.`
            let addr: usize = kani::any();
            let iov_len: usize =
                kani::any_where(|&len| matches!(addr.checked_add(len), Some(x) if x <= size));
            let iov_base = unsafe { mem.offset(addr.try_into().unwrap()) } as *mut c_void;

            vecs.push(iovec { iov_base, iov_len });
            len += iov_len;
        }

        (vecs, len)
    }

    impl kani::Arbitrary for IoVecBuffer {
        fn any() -> Self {
            // We only read from `IoVecBuffer`, so create here a guest memory object, with arbitrary
            // contents and size up to GUEST_MEMORY_SIZE.
            let mut mem = ManuallyDrop::new(kani::vec::exact_vec::<u8, GUEST_MEMORY_SIZE>());
            let (vecs, len) = create_iovecs(mem.as_mut_ptr(), mem.len());
            Self { vecs, len }
        }
    }

    impl kani::Arbitrary for IoVecBufferMut {
        fn any() -> Self {
            // We only write into `IoVecBufferMut` objects, so we can simply create a guest memory
            // object initialized to zeroes, trying to be nice to Kani.
            let mem = unsafe {
                std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(
                    GUEST_MEMORY_SIZE,
                    16,
                ))
            };

            let (vecs, len) = create_iovecs(mem, GUEST_MEMORY_SIZE);
            Self { vecs, len }
        }
    }

    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::solver(cadical)]
    fn verify_read_from_iovec() {
        let iov: IoVecBuffer = kani::any();

        let mut buf = vec![0; GUEST_MEMORY_SIZE];
        let offset: usize = kani::any();

        // We can't really check the contents that the operation here writes into `buf`, because
        // our `IoVecBuffer` being completely arbitrary can contain overlapping memory regions, so
        // checking the data copied is not exactly trivial.
        //
        // What we can verify is the bytes that we read out from guest memory:
        // * `None`, if `offset` is past the guest memory.
        // * `Some(bytes)`, otherwise. In this case, `bytes` is:
        //    - `buf.len()`, if `offset + buf.len() < iov.len()`;
        //    - `iov.len() - offset`, otherwise.
        match iov.read_at(&mut buf, offset) {
            None => assert!(offset >= iov.len()),
            Some(bytes) => assert_eq!(bytes, buf.len().min(iov.len() - offset)),
        }
    }

    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::solver(cadical)]
    fn verify_write_to_iovec() {
        let mut iov_mut: IoVecBufferMut = kani::any();

        let buf = kani::vec::any_vec::<u8, GUEST_MEMORY_SIZE>();
        let offset: usize = kani::any();

        // We can't really check the contents that the operation here writes into `IoVecBufferMut`,
        // because our `IoVecBufferMut` being completely arbitrary can contain overlapping memory
        // regions, so checking the data copied is not exactly trivial.
        //
        // What we can verify is the bytes that we write into guest memory:
        // * `None`, if `offset` is past the guest memory.
        // * `Some(bytes)`, otherwise. In this case, `bytes` is:
        //    - `buf.len()`, if `offset + buf.len() < iov.len()`;
        //    - `iov.len() - offset`, otherwise.
        match iov_mut.write_at(&buf, offset) {
            None => assert!(offset >= iov_mut.len()),
            Some(bytes) => assert_eq!(bytes, buf.len().min(iov_mut.len() - offset)),
        }
    }
}

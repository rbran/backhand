use std::io::{Read, Seek, SeekFrom};

use deku::bitvec::BitView;
use deku::prelude::*;
use tracing::{debug, instrument, trace};

use crate::error::SquashfsError;
use crate::fragment::{Fragment, FRAGMENT_SIZE};
use crate::inode::{BasicDirectory, Inode};
use crate::metadata;
use crate::squashfs::{Export, Id, SuperBlock};

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// Private struct containing logic to read the `Squashfs` section from a file
pub struct SquashfsReader {
    io: Box<dyn ReadSeek>,
    /// Offset from start of file to squashfs
    start: u64,
}

impl SquashfsReader {
    pub fn new<R: ReadSeek + 'static>(reader: R, offset: u64) -> Self {
        Self {
            io: Box::new(reader),
            start: offset,
        }
    }

    /// Offset from start + address
    pub fn stream_position(&mut self) -> Result<u64, SquashfsError> {
        Ok(self.io.stream_position()? - self.start)
    }

    /// Offset from start + address
    pub fn addr(&self, seek: u64) -> Result<u64, SquashfsError> {
        Ok(self.start + seek)
    }

    pub fn seek_from_start(&mut self, seek: u64) -> Result<(), SquashfsError> {
        trace!("seeking: 0x{:02x?}", seek);
        self.io.seek(SeekFrom::Start(self.addr(seek)?))?;
        Ok(())
    }
}

impl SquashfsReader {
    /// Read in entire data and fragments
    #[instrument(skip_all)]
    pub fn data_and_fragments(
        &mut self,
        superblock: &SuperBlock,
    ) -> Result<Vec<u8>, SquashfsError> {
        self.seek_from_start(0)?;
        let mut buf = vec![0u8; superblock.inode_table as usize];
        self.io.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Parse Inode Table into `Vec<(position_read, Inode)>`
    ///
    /// TODO: in the future instead of reacing all the metadatas in this section, we should parse
    /// the dir table ( I think ) that has all inode offset information
    #[instrument(skip_all)]
    pub fn inodes(&mut self, superblock: &SuperBlock) -> Result<Vec<Inode>, SquashfsError> {
        self.seek_from_start(superblock.inode_table)?;

        // The directory inodes store the total, uncompressed size of the entire listing, including headers.
        // Using this size, a SquashFS reader can determine if another header with further entries
        // should be following once it reaches the end of a run.

        // TODO: with capacity?
        let mut ret_bytes = vec![];

        //let og_len = buf.len();
        let mut metadata_offsets = vec![];
        //let mut rest = buf;
        let start = self.stream_position()?;
        while self.stream_position()? < superblock.dir_table {
            trace!(
                "{:02x?} {:02x?}",
                self.stream_position()?,
                self.addr(superblock.dir_table)?
            );
            metadata_offsets.push(self.stream_position()? - start);
            // parse into metadata
            let mut bytes = metadata::read_block(&mut self.io, superblock)?;
            ret_bytes.append(&mut bytes);
        }
        trace!("All bytes read: {:02x?}", ret_bytes);

        // TODO: with capacity?
        let mut ret_vec = vec![];
        while !ret_bytes.is_empty() {
            let input_bits = ret_bytes.view_bits::<deku::bitvec::Msb0>();
            match Inode::read(input_bits, (superblock.block_size, superblock.block_log)) {
                Ok((rest, inode)) => {
                    // Push the new Inode to the return, with the position this was read from
                    trace!("{inode:02x?}");
                    ret_vec.push(inode);
                    ret_bytes = rest.as_raw_slice().to_vec();
                },
                Err(e) => {
                    // TODO: this should return an error
                    tracing::error!("{:02x?} - {}", &ret_bytes[..20], e);
                    break;
                },
            }
        }

        Ok(ret_vec)
    }

    /// Extract the root `Inode` as a `BasicDirectory`
    #[instrument(skip_all)]
    pub fn root_inode(
        &mut self,
        superblock: &SuperBlock,
        //inodes: &[(usize, usize, Inode)],
    ) -> Result<BasicDirectory, SquashfsError> {
        // I think we can always be in one metadata? This assumption is taken with this following
        // code
        let root_inode_start = (superblock.root_inode >> 16) as usize;
        let root_inode_offset = (superblock.root_inode & 0xffff) as usize;
        trace!("root_inode_start:  0x{root_inode_start:02x?}");
        trace!("root_inode_offset: 0x{root_inode_offset:02x?}");

        let seek = superblock.inode_table + root_inode_start as u64;
        self.seek_from_start(seek)?;
        let bytes = metadata::read_block(&mut self.io, superblock)?;
        let new_bytes = &bytes[root_inode_offset..];

        let input_bits = new_bytes.view_bits::<::deku::bitvec::Msb0>();
        match Inode::read(input_bits, (superblock.block_size, superblock.block_log)) {
            Ok((_, inode)) => {
                trace!("{inode:02x?}");
                Ok(inode.expect_dir())
            },
            Err(e) => Err(e.into()),
        }
    }

    /// Parse required number of `Metadata`s uncompressed blocks required for `Dir`s
    #[instrument(skip_all)]
    pub fn dir_blocks(
        &mut self,
        superblock: &SuperBlock,
        inodes: &Vec<Inode>,
    ) -> Result<Vec<(u64, Vec<u8>)>, SquashfsError> {
        let mut max_metadata = 0;
        for inode in inodes {
            // TODO: use match
            if let Inode::BasicDirectory(basic_dir) = inode {
                if basic_dir.block_index > max_metadata {
                    max_metadata = basic_dir.block_index;
                }
            }
            if let Inode::ExtendedDirectory(ex_dir) = inode {
                if ex_dir.block_index > max_metadata {
                    max_metadata = ex_dir.block_index;
                }
            }
        }

        let offset = superblock.dir_table;

        self.metadata_blocks(superblock, offset, u64::from(max_metadata) + 1 + offset)
    }

    /// Parse into Metadata uncompressed blocks
    #[instrument(skip_all)]
    fn metadata_blocks(
        &mut self,
        superblock: &SuperBlock,
        seek: u64,
        max: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, SquashfsError> {
        self.seek_from_start(seek)?;

        let mut all_bytes = vec![];
        // in order to grab a `count` of Metadatas, we can't use Deku for usage of std::io::Read
        while self.stream_position()? < max {
            let metadata_start = self.stream_position()?;
            let bytes = metadata::read_block(&mut self.io, superblock)?;
            all_bytes.push((metadata_start - seek, bytes));
        }

        Ok(all_bytes)
    }

    /// Parse Fragment Table
    #[instrument(skip_all)]
    pub fn fragments(
        &mut self,
        superblock: &SuperBlock,
    ) -> Result<Option<Vec<Fragment>>, SquashfsError> {
        if superblock.frag_count == 0 {
            return Ok(None);
        }
        let fragment = self.lookup_table::<Fragment>(
            superblock,
            superblock.frag_table,
            u64::from(superblock.frag_count) * FRAGMENT_SIZE as u64,
        )?;

        Ok(Some(fragment))
    }

    /// Parse Export Table
    #[instrument(skip_all)]
    pub fn export(
        &mut self,
        superblock: &SuperBlock,
    ) -> Result<Option<Vec<Export>>, SquashfsError> {
        if superblock.nfs_export_table_exists() {
            let ptr = superblock.export_table;
            let count = (superblock.inode_count as f32 / 1024_f32).ceil() as u64;
            let res = self.lookup_table::<Export>(superblock, ptr, count)?;
            Ok(Some(res))
        } else {
            Ok(None)
        }
    }

    /// Parse ID Table
    #[instrument(skip_all)]
    pub fn id(&mut self, superblock: &SuperBlock) -> Result<Option<Vec<Id>>, SquashfsError> {
        if superblock.nfs_export_table_exists() {
            let ptr = superblock.id_table;
            let count = superblock.id_count as u64;
            let res = self.lookup_table::<Id>(superblock, ptr, count)?;
            Ok(Some(res))
        } else {
            Ok(None)
        }
    }

    /// Parse Lookup Table
    #[instrument(skip_all)]
    fn lookup_table<T: for<'a> DekuContainerRead<'a>>(
        &mut self,
        superblock: &SuperBlock,
        seek: u64,
        size: u64,
    ) -> Result<Vec<T>, SquashfsError> {
        debug!(
            "Lookup Table: seek 0x{:02x?}, metadata size: 0x{:02x?}",
            seek, size
        );
        // find the pointer at the initial offset
        self.seek_from_start(seek)?;
        let mut buf = [0u8; 4];
        self.io.read_exact(&mut buf)?;
        let ptr = u32::from_le_bytes(buf);

        let block_count = (size as f32 / 8192_f32).ceil() as u64;

        self.metadata_with_count::<T>(superblock, u64::from(ptr), block_count)
    }

    /// Parse count of `Metadata` block at offset into `T`
    #[instrument(skip_all)]
    fn metadata_with_count<T: for<'a> DekuContainerRead<'a>>(
        &mut self,
        superblock: &SuperBlock,
        seek: u64,
        count: u64,
        //TODO: remove?
    ) -> Result<Vec<T>, SquashfsError> {
        debug!(
            "Metadata with count: seek 0x{:02x?}, count: 0x{:02x?}",
            seek, count
        );
        self.seek_from_start(seek)?;

        let mut all_bytes = vec![];
        // in order to grab a `count` of Metadatas, we can't use Deku for usage of std::io::Read
        for _ in 0..count {
            let mut bytes = metadata::read_block(&mut self.io, superblock)?;
            all_bytes.append(&mut bytes);
        }

        // TODO: with capacity?
        let mut ret_vec = vec![];
        // Read until we fail to turn bytes into `T`
        while let Ok(((rest, _), t)) = T::from_bytes((&all_bytes, 0)) {
            ret_vec.push(t);
            all_bytes = rest.to_vec();
        }

        Ok(ret_vec)
    }
}
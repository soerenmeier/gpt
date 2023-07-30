//! A pure-Rust library to work with GPT partition tables.
//!
//! It provides support for manipulating (R/W) GPT headers and partition
//! tables. Raw disk devices as well as disk images are supported.
//!
//! ```
//! use gpt;
//! use std::convert::TryFrom;
//! use std::io::{Read, Seek};
//!
//! fn inspect_disk() {
//!     let diskpath = std::path::Path::new("/dev/sdz");
//!     let cfg = gpt::GptConfig::new().writable(false);
//!
//!     let disk = cfg.open(diskpath).expect("failed to open disk");
//!
//!     println!("Disk header: {:#?}", disk.primary_header());
//!     println!("Partition layout: {:#?}", disk.partitions());
//! }
//!
//! fn create_partition() {
//!     let diskpath = std::path::Path::new("/tmp/chris.img");
//!     let cfg = gpt::GptConfig::new().writable(true);
//!     let mut disk = cfg.open(diskpath).expect("failed to open disk");
//!     let result = disk.add_partition(
//!         "rust_partition",
//!         100,
//!         gpt::partition_types::LINUX_FS,
//!         0,
//!         None
//!     );
//!     disk.write().unwrap();
//! }
//!
//! /// Demonstrates how to create a new partition table without anything pre-existing
//! fn create_partition_in_ram() {
//!     const TOTAL_BYTES: usize = 1024 * 64;
//!     let mut mem_device = std::io::Cursor::new(vec![0u8; TOTAL_BYTES]);
//!
//!     // Create a protective MBR at LBA0
//!     let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
//!         u32::try_from((TOTAL_BYTES / 512) - 1).unwrap_or(0xFF_FF_FF_FF));
//!     mbr.overwrite_lba0(&mut mem_device).expect("failed to write MBR");
//!
//!     let mut gdisk = gpt::GptConfig::default()
//!         .writable(true)
//!         .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
//!         .create_from_device(mem_device, None)
//!         .expect("failed to crate GptDisk");
//!
//!     // Initialize the headers using a blank partition table
//!     gdisk.update_partitions(
//!         std::collections::BTreeMap::<u32, gpt::partition::Partition>::new()
//!     ).expect("failed to initialize blank partition table");
//!
//!     // At this point, gdisk.primary_header() and gdisk.backup_header() are populated...
//!     // Add a few partitions to demonstrate how...
//!     gdisk.add_partition("test1", 1024 * 12, gpt::partition_types::BASIC, 0, None)
//!         .expect("failed to add test1 partition");
//!     gdisk.add_partition("test2", 1024 * 18, gpt::partition_types::LINUX_FS, 0, None)
//!         .expect("failed to add test2 partition");
//!     // Write the partition table and take ownership of
//!     // the underlying memory buffer-backed block device
//!     let mut mem_device = gdisk.write().expect("failed to write partition table");
//!     // Read the written bytes out of the memory buffer device
//!     mem_device.seek(std::io::SeekFrom::Start(0)).expect("failed to seek");
//!     let mut final_bytes = vec![0u8; TOTAL_BYTES];
//!     mem_device.read_exact(&mut final_bytes)
//!         .expect("failed to read contents of memory device");
//! }
//!
//! // only manipulates memory buffers, so this can run on any system...
//! create_partition_in_ram();
//! ```

#![deny(missing_docs)]

use log::*;
use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};
use std::{fmt, fs, io, path};

#[macro_use]
mod macros;
pub mod disk;
pub mod header;
pub mod mbr;
pub mod partition;
pub mod partition_types;

/// A generic device that we can read/write partitions from/to.
pub trait DiskDevice: Read + Write + Seek + std::fmt::Debug {}
/// Implement the DiskDevice trait for anything that meets the
/// requirements, e.g., `std::fs::File`
impl<T> DiskDevice for T where T: Read + Write + Seek + std::fmt::Debug {}
/// A dynamic trait object that is used by GptDisk for reading/writing/seeking.
pub type DiskDeviceObject<'a> = Box<dyn DiskDevice + 'a>;

#[non_exhaustive]
#[derive(Debug)]
/// Errors returned when interacting with a Gpt Disk.
pub enum GptError {
    /// Generic IO Error
    Io(io::Error),
    /// Error returned from writing or reading the header
    Header(header::HeaderError),
    /// we were expecting to read an existing partition table, but instead we're
    /// attempting to create a new blank table
    CreatingInitializedDisk,
    /// Somthing Overflowed or Underflowed
    /// This will never occur when dealing with sane values
    Overflow(&'static str),
    /// Unable to find enough space on drive
    NotEnoughSpace,
    /// disk not opened in writable mode
    ReadOnly,
    /// If you try to create more partition than the header supports
    OverflowPartitionCount,
    /// The partition count changes but you did not allow that
    PartitionCountWouldChange,
}

impl From<io::Error> for GptError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<header::HeaderError> for GptError {
    fn from(e: header::HeaderError) -> Self {
        Self::Header(e)
    }
}

impl std::error::Error for GptError {}

impl fmt::Display for GptError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        use GptError::*;
        let desc = match self {
            Io(e) => return write!(fmt, "GPT IO Error: {e}"),
            Header(e) => return write!(fmt, "GPT Header Error: {e}"),
            CreatingInitializedDisk => {
                "we were expecting to read an existing \
                partition table, but instead we're attempting to create a \
                new blank table"
            }
            Overflow(m) => return write!(fmt, "GTP error Overflow: {m}"),
            NotEnoughSpace => "Unable to find enough space on drive",
            ReadOnly => "disk not opened in writable mode",
            OverflowPartitionCount => "not enough partition slots",
            PartitionCountWouldChange => "partition would change but is not \
            allowed"
        };
        write!(fmt, "{desc}")
    }
}

/// Configuration options to open a GPT disk.

// write_backup, allow_first_usable_last_usable, change
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GptConfig {
    /// Logical block size.
    lb_size: disk::LogicalBlockSize,
    /// Whether to open a GPT partition table in writable mode.
    writable: bool,
    /// Force both the primary and backup header to be valid
    only_valid_headers: bool,
    /// Treat the backup header as readonly
    readonly_backup: bool,
    /// allows to change the partition count
    ///
    /// ## Warning
    /// This might change the first usable and last usable part
    change_partition_count: bool
}

impl GptConfig {
    /// Create a new default configuration.
    pub fn new() -> Self {
        GptConfig::default()
    }

    /// Whether to open a GPT partition table in writable mode.
    pub fn writable(mut self, writable: bool) -> Self {
        self.writable = writable;
        self
    }

    /// Size of logical blocks (sectors) for this disk.
    pub fn logical_block_size(mut self, lb_size: disk::LogicalBlockSize) -> Self {
        self.lb_size = lb_size;
        self
    }

    /// Sets wether both header need to be valid to open a device.
    pub fn only_valid_headers(mut self, only_valid_headers: bool) -> Self {
        self.only_valid_headers = only_valid_headers;
        self
    }

    /// Sets wether the backup header should be treated as readonly.
    pub fn readonly_backup(mut self, readonly_backup: bool) -> Self {
        self.readonly_backup = readonly_backup;
        self
    }

    /// Sets wether the partition count of the current header can be change.
    ///
    /// ## Warning
    /// This might change the first usable and last usable lba.
    pub fn change_partition_count(mut self, change_partition_count: bool) -> Self {
        self.change_partition_count = change_partition_count;
        self
    }

    /// Open the GPT disk at the given path and inspect it according
    /// to configuration options.
    pub fn open(self, diskpath: impl AsRef<path::Path>) -> Result<GptDisk<fs::File>, GptError> {
        let file = fs::OpenOptions::new()
            .write(self.writable)
            .read(true)
            .open(diskpath)?;
        self.open_from_device(file)
    }

    /// Creates the GPT disk at the given path.
    ///
    /// ## Note
    /// This does not touch the fs until `GptDisk::write` get's called.
    pub fn create(self, diskpath: impl AsRef<path::Path>) -> Result<GptDisk<fs::File>, GptError> {
        let file = fs::OpenOptions::new()
            .write(self.writable)
            .read(true)
            .open(diskpath)?;
        self.create_from_device(file, None)
    }

    /// Open the GPT disk from the given DiskDeviceObject and
    /// inspect it according to configuration options.
    pub fn open_from_device<D>(self, mut device: D) -> Result<GptDisk<D>, GptError>
    where
        D: DiskDevice,
    {
        // Proper GPT disk, fully inspect its layout.
        let h1 = header::read_primary_header(&mut device, self.lb_size);
        let h2 = header::read_backup_header(&mut device, self.lb_size);

        let (h1, h2) = if self.only_valid_headers {
            (Some(h1?), Some(h2?))
        } else if h1.is_err() && h2.is_err() {
            return Err(h1.unwrap_err().into())
        } else {
            (h1.ok(), h2.ok())
        };

        let header = h1.as_ref().or_else(|| h2.as_ref()).unwrap();
        let table = partition::file_read_partitions(&mut device, header, self.lb_size)?;

        let disk = GptDisk {
            config: self,
            device,
            guid: header.disk_guid,
            primary_header: h1,
            backup_header: h2,
            partitions: table
        };
        debug!("disk: {:?}", disk);
        Ok(disk)
    }

    /// Create a GPTDisk with default headers and an empty partition table.
    /// If guid is None then it will generate a new random guid.
    pub fn create_from_device<D>(
        self,
        device: D,
        guid: Option<uuid::Uuid>,
    ) -> Result<GptDisk<D>, GptError>
    where
        D: DiskDevice,
    {
        let mut disk = GptDisk {
            config: self,
            device,
            guid: guid.unwrap_or_else(uuid::Uuid::new_v4),
            primary_header: None,
            backup_header: None,
            partitions: BTreeMap::new(),
        };
        // setup default headers
        disk.init_headers()?;
        Ok(disk)
    }
}

impl Default for GptConfig {
    fn default() -> Self {
        Self {
            lb_size: disk::DEFAULT_SECTOR_SIZE,
            writable: false,
            only_valid_headers: false,
            readonly_backup: false,
            change_partition_count: false
        }
    }
}

/// A GPT disk backed by an arbitrary device.
#[derive(Debug)]
pub struct GptDisk<D> {
    /// if you set config initialized this means there exists a primary_header
    config: GptConfig,
    device: D,
    guid: uuid::Uuid,
    primary_header: Option<header::Header>,
    backup_header: Option<header::Header>,
    partitions: BTreeMap<u32, partition::Partition>,
}

impl<D> GptDisk<D>
where
    D: DiskDevice,
{
    /// Add another partition to this disk.  This tries to find
    /// the optimum partition location with the lowest block device.
    /// Returns the new partition id if there was sufficient room
    /// to add the partition. Size is specified in bytes.
    pub fn add_partition(
        &mut self,
        name: &str,
        size: u64,
        part_type: partition_types::Type,
        flags: u64,
        part_alignment: Option<u64>,
    ) -> Result<u32, GptError> {
        // Ceiling division which avoids overflow
        let size_lba = size
            .checked_sub(1)
            .ok_or(GptError::Overflow("size must be greater than zero bytes"))?
            .checked_div(self.config.lb_size.into())
            .ok_or(GptError::Overflow(
                "invalid logical block size caused bad \
                division when calculating size in blocks",
            ))?
            // we will never divide by 1 so we always have room for one more
            + 1;

        // Find the lowest lba that is larger than size.
        let free_sections = self.find_free_sectors();
        for (starting_lba, length) in free_sections {
            // Get the distance between the starting LBA of this section and the next aligned LBA
            // We don't need to do any checked math here because we guarantee that with `(A % B)`,
            // `A` will always be between 0 and `B-1`.
            let alignment_offset_lba = match part_alignment {
                Some(alignment) => (alignment - (starting_lba % alignment)) % alignment,
                None => 0_u64,
            };

            debug!(
                "starting_lba {}, length {}, alignment_offset_lba {}",
                starting_lba, length, alignment_offset_lba
            );

            if length >= (alignment_offset_lba + size_lba - 1) {
                let starting_lba = starting_lba + alignment_offset_lba;
                // Found our free slice.
                let partition_id = self.find_next_partition_id();
                debug!(
                    "Adding partition id: {} {:?}.  first_lba: {} last_lba: {}",
                    partition_id,
                    part_type,
                    starting_lba,
                    starting_lba + size_lba - 1_u64
                );

                // let's try to increase the num parts
                self.header_update_num_parts(partition_id + 1)?;

                let part = partition::Partition {
                    part_type_guid: part_type,
                    part_guid: uuid::Uuid::new_v4(),
                    first_lba: starting_lba,
                    last_lba: starting_lba + size_lba - 1_u64,
                    flags,
                    name: name.to_string(),
                };
                if let Some(p) = self.partitions.insert(partition_id, part.clone()) {
                    debug!("Replacing\n{}\nwith\n{}", p, part);
                }
                return Ok(partition_id);
            }
        }

        Err(GptError::NotEnoughSpace)
    }
    /// remove partition from this disk. This tries to find the partition based on either a
    /// given partition number (id) or a partition guid.  Returns the partition id if the
    /// partition is removed
    ///
    /// ## Panics
    /// if both are None or both are Some
    pub fn remove_partition(
        &mut self,
        id: Option<u32>,
        partguid: Option<uuid::Uuid>,
    ) -> io::Result<u32> {
        // todo split this into two functions

        assert!((id.is_some() && partguid.is_none()) || (id.is_none() && partguid.is_some()));

        if let Some(part_id) = id {
            if let Some(partition_id) = self.partitions.remove(&part_id) {
                debug!("Removing partition number {}", partition_id);
            }
            return Ok(part_id);
        }
        if let Some(part_guid) = partguid {
            for (key, partition) in &self.partitions.clone() {
                if partition.part_guid == part_guid {
                    if let Some(partition_id) = self.partitions.remove(key) {
                        debug!("Removing partition number {}", partition_id);
                    }
                    return Ok(*key);
                }
            }
        }

        unreachable!()
    }

    /// Find free space on the disk.
    /// Returns a tuple of (starting_lba, length in lba's).
    pub fn find_free_sectors(&self) -> Vec<(u64, u64)> {
        let header = self.header();

        trace!("first_usable: {}", header.first_usable);
        let mut disk_positions = vec![header.first_usable];
        for part in self.partitions().iter().filter(|p| p.1.is_used()) {
            trace!("partition: ({}, {})", part.1.first_lba, part.1.last_lba);
            disk_positions.push(part.1.first_lba);
            disk_positions.push(part.1.last_lba);
        }
        disk_positions.push(header.last_usable);
        trace!("last_usable: {}", header.last_usable);
        disk_positions.sort_unstable();

        disk_positions
            // Walk through the LBA's in chunks of 2 (ending, starting).
            .chunks(2)
            // Add 1 to the ending and then subtract the starting if NOT the first usable sector
            .map(|p| {
                if p[0] == header.first_usable {
                    (p[0], p[1].saturating_sub(p[0]))
                } else {
                    (p[0] + 1, p[1].saturating_sub(p[0] + 1))
                }
            })
            .collect()
    }

    /// Find next highest partition id.
    pub fn find_next_partition_id(&self) -> u32 {
        let max = match self
            .partitions()
            .iter()
            // Skip unused partitions.
            .filter(|p| p.1.is_used())
            // Find the maximum id.
            .max_by_key(|x| x.0)
        {
            Some(i) => *i.0,
            // Partitions start at 1.
            None => return 1,
        };
        for i in 1..max {
            if self.partitions().get(&i).is_none() {
                return i;
            }
        }
        max + 1
    }

    /// Retrieve primary header, if any.
    pub fn primary_header(&self) -> Option<&header::Header> {
        self.primary_header.as_ref()
    }

    /// Retrieve backup header, if any.
    pub fn backup_header(&self) -> Option<&header::Header> {
        self.backup_header.as_ref()
    }

    /// Retrieve the current valid header.
    ///
    /// This can only fail while we're building the disk
    fn try_header(&self) -> Option<&header::Header> {
        self.primary_header
            .as_ref()
            .or_else(|| self.backup_header.as_ref())
    }

    /// Retrieve the current valid header.
    pub fn header(&self) -> &header::Header {
        self.try_header()
            .expect("no primary and no backup header")
    }

    /// Retrieve partition entries.
    pub fn partitions(&self) -> &BTreeMap<u32, partition::Partition> {
        &self.partitions
    }

    /// Retrieve disk UUID.
    pub fn guid(&self) -> &uuid::Uuid {
        &self.guid
    }

    /// Retrieve disk logical block size.
    pub fn logical_block_size(&self) -> &disk::LogicalBlockSize {
        &self.config.lb_size
    }

    /// Change the disk device that we are reading/writing from/to.
    /// Returns the previous disk device.
    pub fn update_disk_device(&mut self, device: D, writable: bool) -> D {
        self.config.writable = writable;
        std::mem::replace(&mut self.device, device)
    }

    /// Updates the disk device that the GptDisk instance is interacting with.
    /// Returns a new GptDisk instance, retaining the previous configuration and GUID,
    /// but with the specified device and writable status.
    pub fn with_disk_device<N>(&self, device: N, writable: bool) -> GptDisk<N> {
        let mut n = GptDisk {
            config: self.config.clone(),
            device,
            guid: self.guid,
            primary_header: self.primary_header.clone(),
            backup_header: self.backup_header.clone(),
            partitions: self.partitions.clone(),
        };
        n.config.writable = writable;

        n
    }

    /// Update disk UUID.
    ///
    /// If no UUID is specified, a new random one is generated.
    /// No changes are recorded to disk until `write()` is called.
    pub fn update_guid(&mut self, uuid: Option<uuid::Uuid>) {
        let guid = match uuid {
            Some(u) => u,
            None => {
                let u = uuid::Uuid::new_v4();
                debug!("Generated random uuid: {}", u);
                u
            }
        };
        self.guid = guid;
    }

    // /// returns the num_parts for the current Disk
    // ///
    // /// you can set partitions_len to zero if you don't know yet
    // fn header_num_parts(&self, partitions_len: usize) -> u32 {
    //     // todo this should probably be done differently
    //     self.primary_header
    //         .as_ref()
    //         .map(|h| h.num_parts)
    //         // we don't wan't to change the num_parts if there exists already a header
    //         // if the num_parts should be changed see update_partitions_embedded
    //         .unwrap_or(partitions_len as u32)
    // }

    /// Update current partition table.
    ///
    /// No changes are recorded to disk until `write()` is called.
    pub fn update_partitions(
        &mut self,
        pp: BTreeMap<u32, partition::Partition>,
    ) -> Result<(), GptError> {
        // TODO(lucab): validate partitions.
        let bak = header::find_backup_lba(&mut self.device, self.config.lb_size)?;

        let num_parts = pp.len() as u32;

        let num_parts_changes = self.try_header().map(|h| h.num_parts_would_change(num_parts)).unwrap_or(false);
        if num_parts_changes && !self.config.change_partition_count {
            return Err(GptError::PartitionCountWouldChange)
        }

        self.partitions = pp;

        self.init_headers()
    }

    /// Makes sure there exists a primary header and if allowed also creates the backup
    /// header.
    pub fn init_headers(&mut self) -> Result<(), GptError> {
        let num_parts = self.partitions.len() as u32;

         let h1 = header::HeaderBuilder::from_maybe_header(self.try_header())
            .num_parts(num_parts)
            .backup_lba(bak)
            .disk_guid(self.guid)
            .primary(true)
            .build(self.config.lb_size)?;
        self.primary_header = Some(h1);

        if !self.config.readonly_backup {
            let h2 = header::HeaderBuilder::from_maybe_header(self.backup_header.as_ref())
                .num_parts(num_parts)
                .backup_lba(bak)
                .disk_guid(self.guid)
                .primary(false)
                .build(self.config.lb_size)?;

            self.backup_header = Some(h2);
        }
    }

    /// Persist state to disk, consuming this disk object.
    ///
    /// This is a destructive action, as it overwrite headers and
    /// partitions entries on disk. All writes are flushed to disk
    /// before returning the underlying DiskDeviceObject.
    pub fn write(mut self) -> Result<D, GptError> {
        self.write_inplace()?;

        Ok(self.device)
    }

    /// Persist state to disk, leaving this disk object intact.
    ///
    /// This is a destructive action, as it overwrites headers
    /// and partitions entries on disk. All writes are flushed
    /// to disk before returning.
    //
    // Primary header and backup header don't need to match.
    // so both need to be checked
    pub fn write_inplace(&mut self) -> Result<(), GptError> {
        if !self.config.writable {
            return Err(GptError::ReadOnly);
        }
        debug!("Computing new headers");
        trace!("old primary header: {:?}", self.primary_header);
        trace!("old backup header: {:?}", self.backup_header);
        let bak = header::find_backup_lba(&mut self.device, self.config.lb_size)?;
        trace!("old backup lba: {}", bak);
        let primary_header = self.primary_header.clone().unwrap();
        // if backup_header is None
        // make sure not to write anything to the backup part
        // since that way we can rollback
        let backup_header = self.backup_header.clone();
        // make sure we have always a backup_header
        // .ok_or_else(|| {
        //     HeaderBuilder::from(primary_header)
        //         .primary(false)
        //         .build(self.config.lb_size)
        //         .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
        // })?;

        // Write all of the used partitions at the start of the partition array.
        let mut next_partition_index = 0;
        for (part_idx, partition) in self
            .partitions()
            .clone()
            .iter()
            .filter(|p| p.1.is_used())
            .enumerate()
        {
            // don't allow us to overflow partition array...
            if part_idx >= primary_header.num_parts as usize {
                return Err(GptError::OverflowPartitionCount);
            }

            // Write to primary partition array
            partition.1.write_to_device(
                &mut self.device,
                part_idx as u64,
                primary_header.part_start,
                self.config.lb_size,
                primary_header.part_size,
            )?;
            // IMPORTANT: must also write it to the backup header if it uses a different
            // area to store the partition array; otherwise backup header will not point
            // to an up to date partition array on disk.
            if let Some(backup_header) = backup_header.as_ref() {
                if part_idx >= backup_header.num_parts as usize {
                    return Err(GptError::OverflowPartitionCount);
                }

                if primary_header.part_start != backup_header.part_start {
                    partition.1.write_to_device(
                        &mut self.device,
                        part_idx as u64,
                        backup_header.part_start,
                        self.config.lb_size,
                        backup_header.part_size,
                    )?;
                }
            }
            next_partition_index = part_idx + 1;
        }

        // Next, write zeros to the rest of the primary/backup partition array
        // (ensures any newly deleted partitions are truly removed from disk, etc.)
        // NOTE: we should never underflow here because of boundary checking in loop above.
        partition::Partition::write_zero_entries_to_device(
            &mut self.device,
            next_partition_index as u64,
            (primary_header.num_parts as u64)
                .checked_sub(next_partition_index as u64)
                .unwrap(),
            primary_header.part_start,
            self.config.lb_size,
            primary_header.part_size,
        )?;
        if let Some(backup_header) = backup_header.as_ref() {
            partition::Partition::write_zero_entries_to_device(
                &mut self.device,
                next_partition_index as u64,
                (backup_header.num_parts as u64)
                    .checked_sub(next_partition_index as u64)
                    .unwrap(),
                backup_header.part_start,
                self.config.lb_size,
                backup_header.part_size,
            )?;
        }

        // todo new headers where created. Why?

        if let Some(backup_header) = backup_header {
            debug!("Writing backup header");
            backup_header.write_backup(&mut self.device, self.config.lb_size)?;
        }
        debug!("Writing primary header");
        primary_header.write_primary(&mut self.device, self.config.lb_size)?;

        self.device.flush()?;

        Ok(())
    }

    /// Take the underlying device object and force
    /// self to drop out of scope.
    ///
    /// Caution: this will abandon any changes that where not written.
    pub fn take_device(self) -> D {
        self.device
    }

    fn header_update_num_parts(&mut self, num_parts: u32) -> Result<(), GptError> {
        let num_parts_overflows = num_parts > self.header().num_parts as usize;

        if num_parts_overflows {
            
        }
        if num_parts > self.header().num_parts {
            return 
        }
    }
}

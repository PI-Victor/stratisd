// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

// Code to handle the backing store of a pool.

use std::path::Path;

use devicemapper::Sectors;

use stratis::{ErrorEnum, StratisError, StratisResult};

use super::super::super::types::{BlockDevTier, DevUuid, PoolUuid};

use super::blockdev::StratBlockDev;
use super::blockdevmgr::{coalesce_blkdevsegs, BlkDevSegment, BlockDevMgr, Segment};

/// Handles the lowest level, base layer of this tier.
#[derive(Debug)]
pub struct DataTier {
    /// Manages the individual block devices
    pub block_mgr: BlockDevMgr,
    /// The list of segments granted by block_mgr and used by dm_device
    pub segments: Vec<BlkDevSegment>,
}

impl DataTier {
    /// Setup a previously existing data layer from the block_mgr and
    /// previously allocated segments.
    pub fn setup(
        block_mgr: BlockDevMgr,
        segments: &[(DevUuid, Sectors, Sectors)],
    ) -> StratisResult<DataTier> {
        let uuid_to_devno = block_mgr.uuid_to_devno();
        let mapper = |triple: &(DevUuid, Sectors, Sectors)| -> StratisResult<BlkDevSegment> {
            let device = uuid_to_devno(triple.0).ok_or_else(|| {
                StratisError::Engine(
                    ErrorEnum::NotFound,
                    format!("missing device for UUUD {:?}", &triple.0),
                )
            })?;
            Ok(BlkDevSegment::new(
                triple.0,
                Segment::new(device, triple.1, triple.2),
            ))
        };
        let segments = segments
            .iter()
            .map(&mapper)
            .collect::<StratisResult<Vec<_>>>()?;

        Ok(DataTier {
            block_mgr,
            segments,
        })
    }

    /// Setup a new DataTier struct from the block_mgr.
    ///
    /// Initially 0 segments are allocated.
    ///
    /// WARNING: metadata changing event
    pub fn new(block_mgr: BlockDevMgr) -> DataTier {
        DataTier {
            block_mgr,
            segments: vec![],
        }
    }

    /// Add the given paths to self. Return UUIDs of the new blockdevs
    /// corresponding to the specified paths.
    /// WARNING: metadata changing event
    pub fn add(
        &mut self,
        pool_uuid: PoolUuid,
        paths: &[&Path],
        force: bool,
    ) -> StratisResult<Vec<DevUuid>> {
        self.block_mgr.add(pool_uuid, paths, force)
    }

    /// Allocate at least request sectors from unallocated segments in
    /// block devices belonging to the data tier. Return true if requested
    /// amount or more was allocated, otherwise, false.
    pub fn alloc(&mut self, request: Sectors) -> bool {
        match self.block_mgr.alloc_space(&[request]) {
            Some(segments) => {
                self.segments = coalesce_blkdevsegs(
                    &self.segments,
                    &segments
                        .iter()
                        .flat_map(|s| s.iter())
                        .cloned()
                        .collect::<Vec<_>>(),
                );
                true
            }
            None => false,
        }
    }

    /// The sum of the lengths of all the sectors that have been mapped to an
    /// upper device.
    #[cfg(test)]
    pub fn capacity(&self) -> Sectors {
        self.segments
            .iter()
            .map(|x| x.segment.length)
            .sum::<Sectors>()
    }

    /// The total size of all the blockdevs combined
    pub fn current_capacity(&self) -> Sectors {
        self.block_mgr.current_capacity()
    }

    /// The number of sectors used for metadata by all the blockdevs
    pub fn metadata_size(&self) -> Sectors {
        self.block_mgr.metadata_size()
    }

    /// Destroy the store. Wipe its blockdevs.
    pub fn destroy(&mut self) -> StratisResult<()> {
        self.block_mgr.destroy_all()
    }

    /// Save the given state to the devices. This action bypasses the DM
    /// device entirely.
    pub fn save_state(&mut self, metadata: &[u8]) -> StratisResult<()> {
        self.block_mgr.save_state(metadata)
    }

    /// Lookup an immutable blockdev by its Stratis UUID.
    pub fn get_blockdev_by_uuid(&self, uuid: DevUuid) -> Option<(BlockDevTier, &StratBlockDev)> {
        self.block_mgr
            .get_blockdev_by_uuid(uuid)
            .and_then(|bd| Some((BlockDevTier::Data, bd)))
    }

    /// Lookup a mutable blockdev by its Stratis UUID.
    pub fn get_mut_blockdev_by_uuid(
        &mut self,
        uuid: DevUuid,
    ) -> Option<(BlockDevTier, &mut StratBlockDev)> {
        self.block_mgr
            .get_mut_blockdev_by_uuid(uuid)
            .and_then(|bd| Some((BlockDevTier::Data, bd)))
    }

    /// Get the blockdevs belonging to this tier
    pub fn blockdevs(&self) -> Vec<(DevUuid, &StratBlockDev)> {
        self.block_mgr.blockdevs()
    }

    pub fn blockdevs_mut(&mut self) -> Vec<(DevUuid, &mut StratBlockDev)> {
        self.block_mgr.blockdevs_mut()
    }
}

#[cfg(test)]
mod tests {

    use uuid::Uuid;

    use super::super::super::tests::{loopbacked, real};

    use super::super::metadata::MIN_MDA_SECTORS;

    use super::*;

    /// Put the data tier through some paces. Make it, alloc a small amount,
    /// add some more blockdevs, allocate enough that the newly added blockdevs
    /// must be allocated from for success.
    fn test_add_and_alloc(paths: &[&Path]) -> () {
        assert!(paths.len() > 1);
        let (paths1, paths2) = paths.split_at(paths.len() / 2);

        let pool_uuid = Uuid::new_v4();

        let mgr = BlockDevMgr::initialize(pool_uuid, paths1, MIN_MDA_SECTORS, false).unwrap();

        let mut data_tier = DataTier::new(mgr);

        // A data_tier w/ some devices but nothing allocated
        let mut current_capacity = data_tier.current_capacity();
        let mut capacity = data_tier.capacity();
        assert_eq!(capacity, Sectors(0));
        assert!(current_capacity != Sectors(0));
        assert_eq!(paths1.len(), data_tier.blockdevs().len());

        let last_request_amount = current_capacity;

        let request_amount = data_tier.block_mgr.avail_space() / 2usize;
        assert!(request_amount != Sectors(0));

        assert!(data_tier.alloc(request_amount));

        // A data tier w/ some amount allocated
        assert!(data_tier.capacity() >= request_amount);
        assert_eq!(data_tier.current_capacity(), current_capacity);
        capacity = data_tier.capacity();

        data_tier.add(pool_uuid, paths2, false).unwrap();

        // A data tier w/ additional blockdevs added
        assert!(data_tier.current_capacity() > current_capacity);
        assert_eq!(data_tier.capacity(), capacity);
        assert_eq!(paths1.len() + paths2.len(), data_tier.blockdevs().len());
        current_capacity = data_tier.current_capacity();

        // Allocate enough to get into the newly added block devices
        assert!(data_tier.alloc(last_request_amount));

        assert!(data_tier.capacity() >= request_amount + last_request_amount);
        assert_eq!(data_tier.current_capacity(), current_capacity);

        data_tier.destroy().unwrap();
    }

    #[test]
    pub fn loop_test_add_and_alloc() {
        loopbacked::test_with_spec(
            loopbacked::DeviceLimits::Range(2, 3, None),
            test_add_and_alloc,
        );
    }

    #[test]
    pub fn real_test_add_and_alloc() {
        real::test_with_spec(
            real::DeviceLimits::AtLeast(2, None, None),
            test_add_and_alloc,
        );
    }

    #[test]
    pub fn travis_test_add_and_alloc() {
        loopbacked::test_with_spec(
            loopbacked::DeviceLimits::Range(2, 3, None),
            test_add_and_alloc,
        );
    }
}

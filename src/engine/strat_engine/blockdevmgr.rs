// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

// Code to handle a collection of block devices.

use std::io;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use devicemapper::{Bytes, Device, Sectors, Segment};
use time::Timespec;
use uuid::Uuid;

use super::super::consts::IEC;
use super::super::errors::{EngineError, EngineResult, ErrorEnum};
use super::super::types::{DevUuid, PoolUuid};

use super::blockdev::BlockDev;
use super::device::blkdev_size;
use super::engine::DevOwnership;
use super::metadata::{BDA, MIN_MDA_SECTORS, StaticHeader, validate_mda_size};
use super::range_alloc::RangeAllocator;
use super::serde_structs::{BlockDevSave, Recordable};

const MIN_DEV_SIZE: Bytes = Bytes(IEC::Gi);

/// Resolve a list of Paths of some sort to a set of unique Devices.
/// Return an IOError if there was a problem resolving any particular device.
pub fn resolve_devices(paths: &[&Path]) -> io::Result<HashSet<Device>> {
    let mut devices = HashSet::new();
    for path in paths {
        devices.insert(try!(Device::from_str(&path.to_string_lossy())));
    }
    Ok(devices)
}


#[derive(Debug)]
pub struct BlockDevMgr {
    block_devs: Vec<BlockDev>,
}

impl BlockDevMgr {
    pub fn new(block_devs: Vec<BlockDev>) -> BlockDevMgr {
        BlockDevMgr { block_devs: block_devs }
    }

    /// Initialize a new BlockDevMgr with specified pool and devices.
    pub fn initialize(pool_uuid: &PoolUuid,
                      paths: &[&Path],
                      mda_size: Sectors,
                      force: bool)
                      -> EngineResult<BlockDevMgr> {
        let devices = try!(resolve_devices(paths));
        Ok(BlockDevMgr::new(try!(initialize(pool_uuid, devices, mda_size, force))))
    }

    /// Obtain a BlockDev by its Device.
    pub fn get_by_device(&self, device: Device) -> Option<&BlockDev> {
        self.block_devs.iter().find(|d| d.device() == &device)
    }

    // Obtain a BlockDev by its UUID.
    pub fn get_by_uuid(&self, uuid: &DevUuid) -> Option<&BlockDev> {
        self.block_devs.iter().find(|d| d.uuid() == uuid)
    }

    pub fn add(&mut self,
               pool_uuid: &PoolUuid,
               paths: &[&Path],
               force: bool)
               -> EngineResult<Vec<PathBuf>> {
        let devices = try!(resolve_devices(paths));
        let bds = try!(initialize(pool_uuid, devices, MIN_MDA_SECTORS, force));
        let bdev_paths = bds.iter().map(|p| p.devnode.clone()).collect();
        for bd in bds {
            self.block_devs.push(bd);
        }
        Ok(bdev_paths)
    }

    pub fn destroy_all(mut self) -> EngineResult<()> {
        for bd in self.block_devs.drain(..) {
            try!(bd.wipe_metadata());
        }
        Ok(())
    }

    // Unused space left on blockdevs
    pub fn avail_space(&self) -> Sectors {
        self.block_devs.iter().map(|bd| bd.available()).sum()
    }

    /// If available space is less than size, return None, else return
    /// the segments allocated.
    pub fn alloc_space(&mut self, size: Sectors) -> Option<Vec<Segment>> {
        let mut needed: Sectors = size;
        let mut segs = Vec::new();

        if self.avail_space() < size {
            return None;
        }

        for mut bd in self.block_devs.iter_mut() {
            if needed == Sectors(0) {
                break;
            }

            let (gotten, r_segs) = bd.request_space(needed);
            segs.extend(r_segs);
            needed = needed - gotten;
        }

        assert_eq!(needed, Sectors(0));

        Some(segs)
    }

    pub fn devnodes(&self) -> Vec<PathBuf> {
        self.block_devs
            .iter()
            .map(|d| d.devnode.clone())
            .collect()
    }

    /// Write the given data to all blockdevs marking with specified time.
    // TODO: Cap # of blockdevs written to, as described in SWDD
    pub fn save_state(&mut self, time: &Timespec, metadata: &[u8]) -> EngineResult<()> {
        // TODO: Do something better than panic when saving to blockdev fails.
        // Panic can occur for a the usual IO reasons, but also:
        // 1. If the timestamp is older than a previously written timestamp.
        // 2. If the variable length metadata is too large.
        for mut bd in self.block_devs.iter_mut() {
            bd.save_state(time, metadata).unwrap();
        }
        Ok(())
    }
}

impl Recordable<HashMap<String, BlockDevSave>> for BlockDevMgr {
    fn record(&self) -> EngineResult<HashMap<String, BlockDevSave>> {

        // This function exists to assist the type-checker. The type-checker
        // was unable to infer the type of the apparently equivalent anonymous
        // closure in Rust version 1.17.0.
        fn mapper(bd: &BlockDev) -> EngineResult<(String, BlockDevSave)> {
            Ok((bd.uuid().simple().to_string(), try!(bd.record())))
        }

        let mut result: HashMap<String, BlockDevSave> = HashMap::new();
        for item in self.block_devs.iter().map(mapper) {
            match item {
                Ok((uuid, save)) => {
                    result.insert(uuid, save);
                }
                Err(err) => return Err(err),
            }
        }
        Ok(result)
    }
}

/// Initialize multiple blockdevs at once. This allows all of them
/// to be checked for usability before writing to any of them.
pub fn initialize(pool_uuid: &PoolUuid,
                  devices: HashSet<Device>,
                  mda_size: Sectors,
                  force: bool)
                  -> EngineResult<Vec<BlockDev>> {

    /// Get device information, returns an error if problem with obtaining
    /// that information.
    /// Returns a tuple with the device's path, its size in bytes,
    /// its ownership as determined by calling determine_ownership(),
    /// and an open File handle, all of which are needed later.
    pub fn dev_info(dev: &Device) -> EngineResult<(PathBuf, Bytes, DevOwnership, File)> {
        let devnode = try!(dev.devnode().ok_or_else(|| {
            EngineError::Engine(ErrorEnum::NotFound,
                                format!("could not get device node from dev {}", dev.dstr()))
        }));

        let mut f = try!(OpenOptions::new().read(true).write(true).open(&devnode));
        let dev_size = try!(blkdev_size(&f));
        let ownership = try!(StaticHeader::determine_ownership(&mut f));

        Ok((devnode, dev_size, ownership, f))
    }

    /// Filter devices for admission to pool based on dev_infos.
    /// If there is an error finding out the info, return that error.
    /// Also, return an error if a device is not appropriate for this pool.
    fn filter_devs<I>(dev_infos: I,
                      pool_uuid: &PoolUuid,
                      force: bool)
                      -> EngineResult<Vec<(Device, (PathBuf, Bytes, File))>>
        where I: Iterator<Item = (Device, EngineResult<(PathBuf, Bytes, DevOwnership, File)>)>
    {
        let mut add_devs = Vec::new();
        for (dev, dev_result) in dev_infos {
            let (devnode, dev_size, ownership, f) = try!(dev_result);
            if dev_size < MIN_DEV_SIZE {
                let error_message = format!("{} too small, minimum {} bytes",
                                            devnode.display(),
                                            MIN_DEV_SIZE);
                return Err(EngineError::Engine(ErrorEnum::Invalid, error_message));
            };
            match ownership {
                DevOwnership::Unowned => add_devs.push((dev, (devnode, dev_size, f))),
                DevOwnership::Theirs => {
                    if !force {
                        let err_str = format!("Device {} appears to belong to another application",
                                              devnode.display());
                        return Err(EngineError::Engine(ErrorEnum::Invalid, err_str));
                    } else {
                        add_devs.push((dev, (devnode, dev_size, f)))
                    }
                }
                DevOwnership::Ours(uuid) => {
                    if *pool_uuid != uuid {
                        let error_str = format!("Device {} already belongs to Stratis pool {}",
                                                devnode.display(),
                                                uuid);
                        return Err(EngineError::Engine(ErrorEnum::Invalid, error_str));
                    } else {
                        // Already in this pool (according to its header)
                        // TODO: Check we already know about it
                        // if yes, ignore. If no, add it w/o initializing?
                    }
                }
            }
        }
        Ok(add_devs)
    }

    try!(validate_mda_size(mda_size));

    let dev_infos = devices.into_iter().map(|d: Device| (d, dev_info(&d)));

    let add_devs = try!(filter_devs(dev_infos, pool_uuid, force));

    let mut bds: Vec<BlockDev> = Vec::new();
    for (dev, (devnode, dev_size, mut f)) in add_devs {

        let bda = BDA::initialize(&mut f,
                                  pool_uuid,
                                  &Uuid::new_v4(),
                                  mda_size,
                                  dev_size.sectors());
        if bda.is_err() {
            let mut unerased_devnodes = Vec::new();
            BDA::wipe(&mut f).unwrap_or_else(|_| unerased_devnodes.push(devnode.clone()));
            for bd in bds.drain(..) {
                let bd_devnode = bd.devnode.clone();
                bd.wipe_metadata()
                    .unwrap_or_else(|_| unerased_devnodes.push(bd_devnode));
            }

            let err_msg = format!("Failed to initialize {:?}", devnode);
            if unerased_devnodes.is_empty() {
                return Err(EngineError::Engine(ErrorEnum::Error, err_msg));
            } else {
                let err_msg = format!("{}, then failed to wipe already initialized devnodes: {:?}",
                                      err_msg,
                                      unerased_devnodes);
                return Err(EngineError::Engine(ErrorEnum::Error, err_msg));
            }
        }

        let bda = bda.expect("!bda.is_err()");
        let allocator = RangeAllocator::new(bda.dev_size(), &[(Sectors(0), bda.size())])
            .expect("bda.size() < bda.dev_size() and single range");

        bds.push(BlockDev::new(dev, devnode, bda, allocator));
    }
    Ok(bds)
}
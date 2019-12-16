/*
 * This Source Code Form is subject to the terms of the Mozilla Public License,
 * v. 2.0. If a copy of the MPL was not distributed with this file, You can
 * obtain one at http://mozilla.org/MPL/2.0/.
 *
 *
 * Copyright 2019 Clemens Lutz, German Research Center for Artificial Intelligence
 * Author: Clemens Lutz <clemens.lutz@dfki.de>
 */

use rustacuda::memory::DeviceCopy;

pub mod cpu_radix_partition;
pub mod gpu_radix_partition;

/// Compute the fanout (i.e., the number of partitions) from the number of radix
/// bits.
fn fanout(radix_bits: u32) -> usize {
    1 << radix_bits
}

/// A key-value tuple.
///
/// The partitioned relation is stored as a collection of `Tuple<K, V>`.
///
/// Note that the struct's layout must be kept in sync with its counterpart in
/// C/C++.
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Tuple<Key: Sized, Value: Sized> {
    pub key: Key,
    pub value: Value,
}

unsafe impl<K, V> DeviceCopy for Tuple<K, V>
where
    K: DeviceCopy,
    V: DeviceCopy,
{
}

/*
 * This Source Code Form is subject to the terms of the Mozilla Public License,
 * v. 2.0. If a copy of the MPL was not distributed with this file, You can
 * obtain one at http://mozilla.org/MPL/2.0/.
 *
 *
 * Copyright 2020-2021 Clemens Lutz
 * Author: Clemens Lutz <lutzcle@cml.li>
 */

#include <gpu_common.h>

// Returns the log2 of the next-lower power of two
__device__ int log2_floor_power_of_two(int x) { return 32 - __clz(x) - 1; }

// Returns the log2 of the next-higher power of two
__device__ int log2_ceil_power_of_two(int x) { return 32 - __clz(x - 1); }

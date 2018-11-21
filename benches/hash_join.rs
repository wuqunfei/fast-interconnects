/*
 * This Source Code Form is subject to the terms of the Mozilla Public License,
 * v. 2.0. If a copy of the MPL was not distributed with this file, You can
 * obtain one at http://mozilla.org/MPL/2.0/.
 *
 *
 * Copyright 2018 German Research Center for Artificial Intelligence (DFKI)
 * Author: Clemens Lutz <clemens.lutz@dfki.de>
 */

extern crate accel;
#[macro_use]
extern crate average;
#[macro_use]
extern crate clap;
extern crate core; // Required by average::concatenate!{} macro
extern crate csv;
extern crate cuda_sys;
#[macro_use]
extern crate error_chain;
extern crate hostname;
extern crate mchj_generator;
extern crate numa_gpu;
#[macro_use]
extern crate serde_derive;
extern crate serde;
extern crate structopt;

use accel::device::{sync, Device};
use accel::event::Event;
use accel::mvec::MVec;
use accel::uvec::UVec;

use average::{Estimate, Max, Min, Quantile, Variance};

use numa_gpu::error::Result;
use numa_gpu::operators::hash_join;
use numa_gpu::runtime::memory::*;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use structopt::StructOpt;

arg_enum! {
    #[derive(Copy, Clone, Debug)]
    enum ArgDataSet {
        Alb,
        Kim,
    }
}

arg_enum! {
    #[derive(Copy, Clone, Debug)]
    enum ArgMemType {
        Unified,
        System,
    }
}

arg_enum! {
    #[derive(Copy, Clone, Debug)]
    enum ArgDeviceType {
        CPU,
        GPU,
    }
}

#[derive(StructOpt)]
#[structopt(
    name = "hash_join",
    about = "A benchmark for the hash join operator"
)]
struct CmdOpt {
    /// Number of times to repeat benchmark
    #[structopt(short = "r", long = "repeat", default_value = "30")]
    repeat: u32,

    /// Output path for measurement files (defaults to current directory)
    #[structopt(
        short = "o",
        long = "out-dir",
        parse(from_os_str),
        default_value = "."
    )]
    out_dir: PathBuf,

    /// Memory type with which to allocate data.
    //   unified: CUDA Unified memory (default)
    //   system: System memory allocated with std::vec::Vec
    #[structopt(
        short = "m",
        long = "mem-type",
        default_value = "Unified",
        raw(
            possible_values = "&ArgMemType::variants()",
            case_insensitive = "true"
        )
    )]
    mem_type: ArgMemType,

    /// Use a pre-defined data set.
    //   alb: Albutiu et al. Massively parallel sort-merge joins"
    //   kim: Kim et al. "Sort vs. hash revisited"
    #[structopt(
        short = "s",
        long = "data-set",
        raw(
            possible_values = "&ArgDataSet::variants()",
            case_insensitive = "true"
        )
    )]
    data_set: Option<ArgDataSet>,

    /// Type of the device.
    #[structopt(
        short = "d",
        long = "device-type",
        default_value = "CPU",
        raw(
            possible_values = "&ArgDeviceType::variants()",
            case_insensitive = "true"
        )
    )]
    device_type: ArgDeviceType,
}

#[derive(Debug, Serialize)]
pub struct DataPoint<'h, 'd> {
    pub hostname: &'h str,
    pub device_type: &'d str,
    pub warm_up: bool,
    pub hash_table_bytes: usize,
    pub build_tuples: usize,
    pub build_bytes: usize,
    pub probe_tuples: usize,
    pub probe_bytes: usize,
    pub join_selectivity: f64,
    pub gpu_ms: f32,
}

fn main() {
    let cmd = CmdOpt::from_args();

    // Generate Kim dataset
    mchj_generator::seed_generator(100);
    let pk = mchj_generator::Relation::new_pk(128 * 10_i32.pow(6), mchj_generator::BuildMode::Seq)
        .expect("Couldn't generate primary keys");
    let fk = mchj_generator::Relation::new_fk_from_pk(&pk, 128 * 10_i32.pow(6))
        .expect("Couldn't generate foreign keys");

    // FIXME: Convert i64 to (i32, i32) key, value pair and support this in hash table
    let mut pk_gpu = match cmd.mem_type {
        ArgMemType::Unified => DerefMem::CudaUniMem(
            UVec::<i64>::new(pk.len()).expect("Couldn't allocate GPU primary keys"),
        ),
        ArgMemType::System => DerefMem::SysMem(vec![0; pk.len()]),
    };

    let mut fk_gpu = match cmd.mem_type {
        ArgMemType::Unified => DerefMem::CudaUniMem(
            UVec::<i64>::new(fk.len()).expect("Couldn't allocate GPU foreign keys"),
        ),
        ArgMemType::System => DerefMem::SysMem(vec![0; fk.len()]),
    };

    pk_gpu
        .iter_mut()
        .by_ref()
        .zip(pk.iter())
        .map(|(gpu, origin)| {
            *gpu = origin.key as i64;
        }).collect::<()>();

    fk_gpu
        .iter_mut()
        .by_ref()
        .zip(fk.iter())
        .map(|(gpu, origin)| {
            *gpu = origin.key as i64;
        }).collect::<()>();

    // Device tuning
    let dev = Device::current().expect("Couldn't get CUDA device");
    let dev_props = dev
        .get_property()
        .expect("Couldn't get CUDA device property map");
    let sm_cores = dev.cores().expect("Couldn't get number of GPU cores");
    let cuda_cores = sm_cores * dev_props.multiProcessorCount as u32;
    let warp_size = dev_props.warpSize as u32;
    let overcommit_factor = 8;

    let block_size = warp_size;
    let grid_size = cuda_cores * overcommit_factor / warp_size;

    let hjb = HashJoinBench {
        hash_table_size: 2 * 128 * 2_usize.pow(20),
        build_relation: pk_gpu.into(),
        probe_relation: fk_gpu.into(),
        join_selectivity: 1.0,
        build_dim: (grid_size, block_size),
        probe_dim: (grid_size, block_size),
    };

    let dev_type_str = cmd.device_type.to_string();
    let dp = DataPoint {
        hostname: "",
        device_type: dev_type_str.as_str(),
        warm_up: false,
        hash_table_bytes: hjb.hash_table_size * 16,
        build_tuples: hjb.build_relation.len(),
        build_bytes: hjb.build_relation.len() * 8,
        probe_tuples: hjb.probe_relation.len(),
        probe_bytes: hjb.probe_relation.len() * 8,
        join_selectivity: 1.0,
        gpu_ms: 0.0,
    };

    // Decide which closure to run
    let dev_type = cmd.device_type.clone();
    let hjc = || match dev_type {
        ArgDeviceType::CPU => hjb.cpu_hash_join(),
        ArgDeviceType::GPU => hjb.cuda_hash_join(),
    };

    // Run experiment
    measure("hash_join_kim", cmd.repeat, cmd.out_dir, dp, hjc)
        .expect("Failure: hash join benchmark");
}

fn measure<F>(name: &str, repeat: u32, out_dir: PathBuf, template: DataPoint, func: F) -> Result<()>
where
    F: Fn() -> Result<f32>,
{
    let hostname = &hostname::get_hostname().ok_or_else(|| "Couldn't get hostname")?;

    // FIXME: hard-coded unit sizes
    let measurements = (0..repeat)
        .map(|_| {
            func().map(|gpu_ms| DataPoint {
                hostname,
                warm_up: false,
                gpu_ms,
                ..template
            })
        }).collect::<Result<Vec<_>>>()?;

    let csv_path = out_dir.with_file_name(name).with_extension("csv");

    let csv_file = std::fs::File::create(csv_path)?;

    let mut csv = csv::Writer::from_writer(csv_file);
    ensure!(
        measurements
            .iter()
            .try_for_each(|row| csv.serialize(row))
            .is_ok(),
        "Couldn't write serialized measurements"
    );

    concatenate!(
        Estimator,
        [Variance, variance, mean, error],
        [Quantile, quantile, quantile],
        [Min, min, min],
        [Max, max, max]
    );

    let time_stats: Estimator = measurements.iter().map(|row| row.gpu_ms as f64).collect();

    let tput_stats: Estimator = measurements
        .iter()
        .map(|row| (row.probe_bytes as f64, row.gpu_ms as f64))
        .map(|(bytes, ms)| bytes / ms / 2.0_f64.powf(30.0) * 10.0_f64.powf(3.0))
        .collect();

    println!(
        r#"Bench: {}
Sample size: {}
               Time            Throughput
                ms              GiB/s
Mean:          {:6.2}          {:6.2}
Stddev:        {:6.2}          {:6.2}
Median:        {:6.2}          {:6.2}
Min:           {:6.2}          {:6.2}
Max:           {:6.2}          {:6.2}"#,
        name.replace("_", " "),
        measurements.len(),
        time_stats.mean(),
        tput_stats.mean(),
        time_stats.error(),
        tput_stats.error(),
        time_stats.quantile(),
        tput_stats.quantile(),
        time_stats.min(),
        tput_stats.min(),
        time_stats.max(),
        tput_stats.max(),
    );

    Ok(())
}

struct HashJoinBench {
    hash_table_size: usize,
    build_relation: Mem<i64>,
    probe_relation: Mem<i64>,
    join_selectivity: f64,
    build_dim: (u32, u32),
    probe_dim: (u32, u32),
}

impl HashJoinBench {
    fn cuda_hash_join(&self) -> Result<f32> {
        let hash_table_mem = CudaDevMem(MVec::<i64>::new(self.hash_table_size)?);
        let hash_table = hash_join::HashTable::new(hash_table_mem, self.hash_table_size)?;
        let mut build_selection_attr = CudaUniMem(UVec::<i64>::new(self.build_relation.len())?);
        let mut result_counts = CudaUniMem(UVec::<u64>::new(
            (self.probe_dim.0 * self.probe_dim.1) as usize,
        )?);
        let mut probe_selection_attr = CudaUniMem(UVec::<i64>::new(self.probe_relation.len())?);

        // Initialize counts
        if let CudaUniMem(ref mut c) = result_counts {
            c.iter_mut().map(|count| *count = 0).collect::<()>();
        }

        // Set build selection attributes to 100% selectivity
        if let CudaUniMem(ref mut a) = build_selection_attr {
            a.iter_mut().map(|x| *x = 2).collect::<()>();
        }

        // Set probe selection attributes to 100% selectivity
        if let CudaUniMem(ref mut a) = probe_selection_attr {
            a.iter_mut().map(|x| *x = 2).collect::<()>();
        }

        let mut hj_op = hash_join::CudaHashJoinBuilder::default()
            .build_dim(self.build_dim.0, self.build_dim.1)
            .probe_dim(self.probe_dim.0, self.probe_dim.1)
            .hash_table(hash_table)
            .build()?;

        let start_event = Event::new()?;
        let stop_event = Event::new()?;

        start_event.record()?;

        hj_op
            .build(&self.build_relation, &build_selection_attr)?
            .probe_count(
                &self.probe_relation,
                &probe_selection_attr,
                &mut result_counts,
            )?;

        stop_event.record().and_then(|e| e.synchronize())?;
        let millis = stop_event.elapsed_time(&start_event)?;

        sync()?;
        Ok(millis)
    }

    fn cpu_hash_join(&self) -> Result<f32> {
        let hash_table_mem = SysMem(vec![0; self.hash_table_size]);
        let hash_table = hash_join::HashTable::new(hash_table_mem, self.hash_table_size)?;
        let mut result_counts = vec![0; (self.probe_dim.0 * self.probe_dim.1) as usize];
        let build_selection_attr: Vec<_> = (2_i64..).take(self.build_relation.len()).collect();
        let probe_selection_attr: Vec<_> = (2_i64..).take(self.probe_relation.len()).collect();

        let mut hj_op = hash_join::CpuHashJoinBuilder::new_with_ht(Arc::new(hash_table)).build();

        let timer = Instant::now();

        hj_op
            .build(
                match self.build_relation {
                    Mem::CudaUniMem(ref m) => m.as_slice(),
                    Mem::SysMem(ref m) => m.as_slice(),
                    Mem::CudaDevMem(_) => panic!("Can't use CUDA device memory on CPU!"),
                },
                build_selection_attr.as_slice(),
            )?.probe_count(
                match self.probe_relation {
                    Mem::CudaUniMem(ref m) => m.as_slice(),
                    Mem::SysMem(ref m) => m.as_slice(),
                    Mem::CudaDevMem(_) => panic!("Can't use CUDA device memory on CPU!"),
                },
                probe_selection_attr.as_slice(),
                &mut result_counts[0],
            )?;

        let dur = timer.elapsed();
        let millis = dur.as_secs() * 1000 + dur.subsec_millis() as u64;
        Ok(millis as f32)
    }
}

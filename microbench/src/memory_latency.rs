extern crate accel;
extern crate csv;
extern crate cuda_sys;
extern crate hostname;
extern crate nvml_wrapper;
extern crate serde;

use self::accel::device::{sync, Device};
use self::accel::error::Check;
use self::accel::UVec;

use self::cuda_sys::cudart::cudaMemPrefetchAsync;

use self::nvml_wrapper::{enum_wrappers::device::Clock, NVML};

use std;
use std::mem::{size_of, size_of_val};
use std::ops::RangeInclusive;
use std::os::raw::c_void;

use crate::types::*;
use crate::utils::hw_info::cpu_codename;
use crate::utils::memory::*;
use crate::utils::numa::{run_on_node, NumaMemory};

extern "C" {
    pub fn gpu_stride(data: *mut u32, iterations: u32);
    pub fn cpu_stride(data: *const u32, iterations: u32) -> u64;
}

pub struct MemoryLatency;

impl MemoryLatency {
    pub fn measure<W>(
        device_id: DeviceId,
        mem_loc: MemoryLocation,
        range: RangeInclusive<usize>,
        stride: RangeInclusive<usize>,
        repeat: u32,
        writer: Option<&mut W>,
    ) where
        W: std::io::Write,
    {
        let buffer_bytes = *range.end() + 1;
        let element_bytes = size_of::<u32>();
        let buffer_len = buffer_bytes / element_bytes;

        let hostname = hostname::get_hostname().expect("Couldn't get hostname");
        let device_type = match device_id {
            DeviceId::Cpu(_) => "CPU",
            DeviceId::Gpu(_) => "GPU",
        };
        let device_codename = match device_id {
            DeviceId::Cpu(_) => cpu_codename(),
            DeviceId::Gpu(_) => Device::current()
                .expect("Couldn't get current device")
                .name()
                .expect("Couldn't get device code name"),
        };
        let memory_node = match device_id {
            DeviceId::Cpu(node) => Some(node),
            _ => None,
        };

        let template = DataPoint {
            hostname: hostname.as_str(),
            device_type,
            device_codename: device_codename.as_str(),
            memory_node,
            ..Default::default()
        };

        let mnt = Measurement::new(range, stride, template);

        let mut mem = match mem_loc {
            MemoryLocation::Unified => DerefMem::CudaUniMem(
                UVec::<u32>::new(buffer_len).expect("Couldn't allocate CUDA memory"),
            ),
            MemoryLocation::System(node) => {
                NumaMemory::<u32>::set_strict();
                DerefMem::NumaMem(NumaMemory::alloc_on_node(buffer_len, node))
            }
        };

        let latencies = match device_id {
            DeviceId::Cpu(did) => {
                let ml = CpuMemoryLatency::new(did);
                mnt.measure(
                    mem.as_mut_slice(),
                    ml,
                    CpuMemoryLatency::prepare,
                    CpuMemoryLatency::run,
                    repeat,
                )
            }
            DeviceId::Gpu(did) => {
                Device::set(did).expect("Cannot set CUDA device. Perhaps CUDA is not installed?");

                let ml = GpuMemoryLatency::new(did);
                let prepare = match mem_loc {
                    MemoryLocation::Unified => GpuMemoryLatency::prepare_prefetch,
                    MemoryLocation::System(_) => GpuMemoryLatency::prepare,
                };
                mnt.measure(
                    mem.as_mut_slice(),
                    ml,
                    prepare,
                    GpuMemoryLatency::run,
                    repeat,
                )
            }
        };

        if let Some(w) = writer {
            let mut csv = csv::Writer::from_writer(w);
            latencies
                .iter()
                .try_for_each(|row| csv.serialize(row))
                .expect("Couldn't write serialized measurements")
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct DataPoint<'h, 'd, 'c> {
    pub hostname: &'h str,
    pub device_type: &'d str,
    pub device_codename: &'c str,
    pub memory_node: Option<u16>,
    pub warm_up: bool,
    pub range_bytes: usize,
    pub stride_bytes: usize,
    pub iterations: u32,
    pub cycles: u64,
    pub ns: u64,
}

#[derive(Debug)]
struct Measurement<'h, 'd, 'c> {
    stride: RangeInclusive<usize>,
    range: RangeInclusive<usize>,
    template: DataPoint<'h, 'd, 'c>,
}

#[derive(Debug)]
struct GpuMemoryLatency {
    device_id: i32,
    nvml: nvml_wrapper::NVML,
}

#[derive(Debug)]
struct CpuMemoryLatency {
    device_id: u16,
}

#[derive(Debug)]
struct MeasurementParameters {
    range: usize,
    stride: usize,
    iterations: u32,
}

impl<'h, 'd, 'c> Measurement<'h, 'd, 'c> {
    fn new(
        range: RangeInclusive<usize>,
        stride: RangeInclusive<usize>,
        template: DataPoint<'h, 'd, 'c>,
    ) -> Self {
        Self {
            stride,
            range,
            template,
        }
    }

    fn measure<P, R, S>(
        &self,
        mem: &mut [u32],
        mut state: S,
        prepare: P,
        run: R,
        repeat: u32,
    ) -> Vec<DataPoint>
    where
        P: Fn(&mut S, &mut [u32], &MeasurementParameters),
        R: Fn(&mut S, &mut [u32], &MeasurementParameters) -> (u64, u64),
    {
        let stride_iter = self.stride.clone();
        let range_iter = self.range.clone();

        let latencies = stride_iter
            .filter(|stride| stride.is_power_of_two())
            .flat_map(|stride| {
                range_iter
                    .clone()
                    .filter(|range| range.is_power_of_two())
                    .zip(std::iter::repeat(stride))
                    .enumerate()
            })
            .flat_map(|(i, (range, stride))| {
                let iterations = (range / stride) as u32;
                let mut data_points: Vec<DataPoint> = Vec::with_capacity(repeat as usize + 1);
                let mut warm_up = true;

                let mp = MeasurementParameters {
                    range,
                    stride,
                    iterations,
                };

                if i == 0 {
                    prepare(&mut state, mem, &mp);
                }

                for _ in 0..repeat + 1 {
                    let (cycles, ns) = run(&mut state, mem, &mp);

                    data_points.push(DataPoint {
                        warm_up,
                        range_bytes: range,
                        stride_bytes: stride,
                        iterations,
                        cycles,
                        ns,
                        ..self.template
                    });
                    warm_up = false;
                }

                data_points
            })
            .collect::<Vec<_>>();

        latencies
    }
}

impl GpuMemoryLatency {
    fn new(device_id: i32) -> Self {
        let nvml = NVML::init().expect("Couldn't initialize NVML");

        Self { device_id, nvml }
    }

    fn prepare(_state: &mut Self, mem: &mut [u32], mp: &MeasurementParameters) {
        write_strides(mem, mp.stride);
    }

    fn prepare_prefetch(state: &mut Self, mem: &mut [u32], mp: &MeasurementParameters) {
        write_strides(mem, mp.stride);

        let pmap = Device::current()
            .expect("Couldn't get current CUDA device")
            .get_property()
            .expect("Couldn't get CUDA device property map");

        // Prefetch data to GPU
        if pmap.concurrentManagedAccess != 0 {
            let element_bytes = size_of_val(&mem[0]);
            unsafe {
                cudaMemPrefetchAsync(
                    mem.as_ptr() as *const c_void,
                    mem.len() * element_bytes,
                    state.device_id,
                    std::mem::zeroed(),
                )
            }
            .check()
            .unwrap();
            sync().unwrap();
        }
    }

    fn run(state: &mut Self, mem: &mut [u32], mp: &MeasurementParameters) -> (u64, u64) {
        // Refresh first values that we override with result
        let element_bytes = size_of_val(&mem[0]);
        mem[0] = (mp.stride / element_bytes) as u32;

        // Launch GPU code
        unsafe { gpu_stride(mem.as_mut_ptr(), mp.iterations) };

        sync().unwrap();

        // Get GPU clock rate that applications run at
        let clock_rate_mhz = state
            .nvml
            .device_by_index(state.device_id as u32)
            .expect("Couldn't get NVML device")
            .clock_info(Clock::SM)
            .expect("Couldn't get clock rate with NVML");

        let cycles: u64 = mem[0] as u64;
        let ns: u64 = cycles * 1000 / (clock_rate_mhz as u64);

        (cycles, ns)
    }
}

impl CpuMemoryLatency {
    fn new(device_id: u16) -> Self {
        NumaMemory::<u32>::set_strict();
        run_on_node(device_id);

        Self { device_id }
    }

    fn run(_state: &mut Self, mem: &mut [u32], mp: &MeasurementParameters) -> (u64, u64) {
        // Launch CPU code
        let ns = unsafe { cpu_stride(mem.as_ptr(), mp.iterations) };

        let cycles = 0;

        (cycles, ns)
    }

    fn prepare(_state: &mut Self, mem: &mut [u32], mp: &MeasurementParameters) {
        write_strides(mem, mp.stride);
    }
}

fn write_strides(data: &mut [u32], stride: usize) -> usize {
    let element_bytes = size_of_val(&data[0]);
    let len = data.len();

    let number_of_strides = data
        .iter_mut()
        .zip((stride / element_bytes)..)
        .map(|(it, next)| *it = (next % len) as u32)
        .count();

    number_of_strides
}

#include <helper.h>

#include <cstdint>

// X mod Y, assuming that Y is a power of 2
#define FAST_MODULO(X, Y) (X & (Y - 1))

__global__ void gpu_stride_kernel(uint32_t *data, uint32_t iterations)
{
    uint64_t sum = 0;
    uint64_t start = 0;
    uint64_t stop = 0;
    uint32_t pos = 0;
    uint32_t dependency = 0; // Prevent compiler from optimizing away the loop

    // Warm-up the cache
    for (uint32_t i = 0; i < iterations; ++i) {

        pos = data[pos];
        dependency += pos;

    }

    // Prevent optimization and reset position
    if (pos != 0) {
        pos = 0;
    }

    start = clock64();

    // Do measurement
    for (uint32_t i = 0; i < iterations; ++i) {

        pos = data[pos];
        dependency += pos;

    }

    stop = clock64();
    sum += stop - start;

    // Write result
    data[0] = (uint32_t)(sum / ((uint64_t)iterations));

    // Prevent compiler optimization
    if (pos == 1) {
        data[1] = dependency;
    }
}

extern "C" void gpu_stride(uint32_t *data, uint32_t iterations)
{
    gpu_stride_kernel<<<1,1>>>(data, iterations);
}

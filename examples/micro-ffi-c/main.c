#include <stdint.h>
#include <stdio.h>

uint64_t mix_hash(uint64_t x, uint64_t y, int rounds);

int main(void) {
    uint64_t acc = 0;
    for (uint64_t i = 0; i < 300000; i++) {
        acc ^= mix_hash(i + 1, acc ^ 0x123456789abcdef0ULL, 64);
    }
    printf("acc=%llu\n", (unsigned long long) acc);
    return 0;
}

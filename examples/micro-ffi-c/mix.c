#include <stdint.h>

uint64_t mix_hash(uint64_t x, uint64_t y, int rounds) {
    uint64_t h = x ^ 0x9e3779b97f4a7c15ULL;
    for (int i = 0; i < rounds; i++) {
        h ^= y + 0x517cc1b727220a95ULL + (h << 6) + (h >> 2);
        h = (h * 0xbf58476d1ce4e5b9ULL) ^ (h >> 31);
        h = (h * 0x94d049bb133111ebULL) ^ (h >> 27);
    }
    return h;
}

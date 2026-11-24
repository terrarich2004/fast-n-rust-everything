pub fn mix_hash(x: u64, y: u64, rounds: i32) -> Result<u64, String> {
    let mut h = x ^ 0x9e37_79b9_7f4a_7c15_u64;
    let steps = rounds.max(0) as u32;
    for _ in 0..steps {
        h ^= y
            .wrapping_add(0x517c_c1b7_2722_0a95)
            .wrapping_add(h << 6)
            .wrapping_add(h >> 2);
        h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9) ^ (h >> 31);
        h = h.wrapping_mul(0x94d0_49bb_1331_11eb) ^ (h >> 27);
    }
    Ok(h)
}

// ---- Compact binary wire v1 for fnr C++ bridge (C++14). Generated; don't edit.
#include <array>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <map>
#include <string>
#include <type_traits>
#include <unordered_map>
#include <utility>
#include <vector>

inline bool bin_utf8(const std::string &s) {
    size_t i = 0;
    while (i < s.size()) {
        unsigned char c = (unsigned char) s[i];
        int n = c < 0x80 ? 0 : (c >> 5) == 6 ? 1 : (c >> 4) == 14 ? 2 : (c >> 3) == 30 ? 3 : -1;
        if (n < 0 || i + (size_t) n >= s.size() + (n == 0)) return n == 0;
        for (int k = 1; k <= n; k++)
            if (((unsigned char) s[i + k] & 0xC0) != 0x80) return false;
        i += (size_t) n + 1;
    }
    return true;
}

inline void bin_u32(std::vector<uint8_t> &o, uint32_t v) {
    uint8_t b[4] = {(uint8_t) v, (uint8_t) (v >> 8), (uint8_t) (v >> 16), (uint8_t) (v >> 24)};
    o.insert(o.end(), b, b + 4);
}
inline void bin_i32(std::vector<uint8_t> &o, int32_t v) { bin_u32(o, (uint32_t) v); }
inline void bin_i64(std::vector<uint8_t> &o, int64_t v) {
    uint64_t u = (uint64_t) v;
    for (int k = 0; k < 8; k++) o.push_back((uint8_t) ((u >> (k * 8)) & 0xFF));
}
inline void bin_f64(std::vector<uint8_t> &o, double v) {
    uint64_t u = 0;
    std::memcpy(&u, &v, 8);
    bin_i64(o, (int64_t) u);
}

inline bool bin_enc(std::vector<uint8_t> &o, bool v) { o.push_back(v ? 2 : 1); return true; }
template <class T>
typename std::enable_if<std::is_integral<T>::value && std::is_signed<T>::value && sizeof(T) <= 4, bool>::type
bin_enc(std::vector<uint8_t> &o, T v) {
    o.push_back(11);
    bin_i32(o, (int32_t) v);
    return true;
}
template <class T>
typename std::enable_if<std::is_integral<T>::value && std::is_signed<T>::value && (sizeof(T) > 4), bool>::type
bin_enc(std::vector<uint8_t> &o, T v) {
    if ((int64_t) v >= INT32_MIN && (int64_t) v <= INT32_MAX) {
        o.push_back(11);
        bin_i32(o, (int32_t) v);
    } else {
        o.push_back(3);
        bin_i64(o, (int64_t) v);
    }
    return true;
}
template <class T>
typename std::enable_if<std::is_integral<T>::value && !std::is_signed<T>::value, bool>::type
bin_enc(std::vector<uint8_t> &o, T v) {
    if ((uint64_t) v <= (uint64_t) INT32_MAX) {
        o.push_back(11);
        bin_i32(o, (int32_t) v);
    } else if ((uint64_t) v <= (uint64_t) INT64_MAX) {
        o.push_back(3);
        bin_i64(o, (int64_t) v);
    } else {
        o.push_back(12);
        bin_i64(o, (int64_t) v);
    }
    return true;
}
template <class T>
typename std::enable_if<std::is_floating_point<T>::value, bool>::type
bin_enc(std::vector<uint8_t> &o, T v) {
    double d = (double) v;
    if (!std::isfinite(d)) return false;
    o.push_back(4);
    bin_f64(o, d);
    return true;
}
inline bool bin_enc(std::vector<uint8_t> &o, const std::string &s) {
    if (!bin_utf8(s)) return false;
    o.push_back(5);
    bin_u32(o, (uint32_t) s.size());
    o.insert(o.end(), s.begin(), s.end());
    return true;
}
inline bool bin_enc(std::vector<uint8_t> &o, const char *s) {
    if (!s) { o.push_back(0); return true; }
    return bin_enc(o, std::string(s));
}
template <class A, class B> bool bin_enc(std::vector<uint8_t> &o, const std::pair<A, B> &v);
template <class T, class Al> bool bin_enc(std::vector<uint8_t> &o, const std::vector<T, Al> &v);
template <class T, size_t N> bool bin_enc(std::vector<uint8_t> &o, const std::array<T, N> &v);
template <class T, class C, class Al> bool bin_enc(std::vector<uint8_t> &o, const std::map<std::string, T, C, Al> &v);
template <class T, class H, class E, class Al> bool bin_enc(std::vector<uint8_t> &o, const std::unordered_map<std::string, T, H, E, Al> &v);

template <class A, class B>
bool bin_enc(std::vector<uint8_t> &o, const std::pair<A, B> &v) {
    o.push_back(6);
    bin_u32(o, 2);
    return bin_enc(o, v.first) && bin_enc(o, v.second);
}
template <class T, class Al>
bool bin_enc(std::vector<uint8_t> &o, const std::vector<T, Al> &v) {
    o.push_back(6);
    bin_u32(o, (uint32_t) v.size());
    for (size_t i = 0; i < v.size(); i++) {
        T item = v[i];
        if (!bin_enc(o, item)) return false;
    }
    return true;
}
template <class Al>
inline bool bin_enc(std::vector<uint8_t> &o, const std::vector<int32_t, Al> &v) {
    o.push_back(8);
    bin_u32(o, (uint32_t) v.size());
    if (!v.empty()) {
        size_t pos = o.size();
        o.resize(pos + v.size() * 4);
        std::memcpy(o.data() + pos, v.data(), v.size() * 4);
    }
    return true;
}
template <class Al>
inline bool bin_enc(std::vector<uint8_t> &o, const std::vector<int64_t, Al> &v) {
    o.push_back(9);
    bin_u32(o, (uint32_t) v.size());
    if (!v.empty()) {
        size_t pos = o.size();
        o.resize(pos + v.size() * 8);
        std::memcpy(o.data() + pos, v.data(), v.size() * 8);
    }
    return true;
}
template <class Al>
inline bool bin_enc(std::vector<uint8_t> &o, const std::vector<double, Al> &v) {
    for (double d : v) if (!std::isfinite(d)) return false;
    o.push_back(10);
    bin_u32(o, (uint32_t) v.size());
    if (!v.empty()) {
        size_t pos = o.size();
        o.resize(pos + v.size() * 8);
        std::memcpy(o.data() + pos, v.data(), v.size() * 8);
    }
    return true;
}
template <class T, size_t N>
bool bin_enc(std::vector<uint8_t> &o, const std::array<T, N> &v) {
    o.push_back(6);
    bin_u32(o, (uint32_t) N);
    for (const auto &x : v) if (!bin_enc(o, x)) return false;
    return true;
}
template <class M>
bool bin_enc_map(std::vector<uint8_t> &o, const M &m) {
    o.push_back(7);
    bin_u32(o, (uint32_t) m.size());
    for (const auto &kv : m) {
        if (!bin_utf8(kv.first)) return false;
        bin_u32(o, (uint32_t) kv.first.size());
        o.insert(o.end(), kv.first.begin(), kv.first.end());
        if (!bin_enc(o, kv.second)) return false;
    }
    return true;
}
template <class T, class C, class Al>
bool bin_enc(std::vector<uint8_t> &o, const std::map<std::string, T, C, Al> &v) { return bin_enc_map(o, v); }
template <class T, class H, class E, class Al>
bool bin_enc(std::vector<uint8_t> &o, const std::unordered_map<std::string, T, H, E, Al> &v) { return bin_enc_map(o, v); }

inline bool bin_enc_seq(std::vector<uint8_t> &) { return true; }
template <class A, class... R>
bool bin_enc_seq(std::vector<uint8_t> &o, const A &a, const R &...r) {
    return bin_enc(o, a) && bin_enc_seq(o, r...);
}
template <class... Args>
bool bin_enc_args(std::vector<uint8_t> &o, const Args &...args) {
    o.push_back(6);
    bin_u32(o, (uint32_t) sizeof...(args));
    return bin_enc_seq(o, args...);
}

inline bool dec_u8(const uint8_t *&p, const uint8_t *e, uint8_t &out) {
    if (p >= e) return false;
    out = *p++;
    return true;
}
inline bool dec_u32(const uint8_t *&p, const uint8_t *e, uint32_t &out) {
    if (e - p < 4) return false;
    std::memcpy(&out, p, 4);
    p += 4;
    return true;
}
inline bool dec_i32(const uint8_t *&p, const uint8_t *e, int32_t &out) {
    if (e - p < 4) return false;
    std::memcpy(&out, p, 4);
    p += 4;
    return true;
}
inline bool dec_i64(const uint8_t *&p, const uint8_t *e, int64_t &out) {
    if (e - p < 8) return false;
    std::memcpy(&out, p, 8);
    p += 8;
    return true;
}
inline bool dec_f64(const uint8_t *&p, const uint8_t *e, double &out) {
    if (e - p < 8) return false;
    std::memcpy(&out, p, 8);
    p += 8;
    return true;
}

inline bool bin_dec(const uint8_t *&p, const uint8_t *e, bool &out) {
    uint8_t t = 0;
    if (!dec_u8(p, e, t)) return false;
    if (t == 1) { out = false; return true; }
    if (t == 2) { out = true; return true; }
    return false;
}
template <class T>
typename std::enable_if<std::is_integral<T>::value, bool>::type
bin_dec(const uint8_t *&p, const uint8_t *e, T &out) {
    uint8_t t = 0;
    if (!dec_u8(p, e, t)) return false;
    if (t == 11) { int32_t v = 0; if (!dec_i32(p, e, v)) return false; out = (T) v; return true; }
    if (t == 3) { int64_t v = 0; if (!dec_i64(p, e, v)) return false; out = (T) v; return true; }
    if (t == 12) { int64_t v = 0; if (!dec_i64(p, e, v)) return false; out = (T) (uint64_t) v; return true; }
    if (t == 4) { double v = 0; if (!dec_f64(p, e, v)) return false; out = (T) v; return true; }
    return false;
}
template <class T>
typename std::enable_if<std::is_floating_point<T>::value, bool>::type
bin_dec(const uint8_t *&p, const uint8_t *e, T &out) {
    uint8_t t = 0;
    if (!dec_u8(p, e, t)) return false;
    if (t == 4) { double v = 0; if (!dec_f64(p, e, v)) return false; out = (T) v; return true; }
    if (t == 11) { int32_t v = 0; if (!dec_i32(p, e, v)) return false; out = (T) v; return true; }
    if (t == 3) { int64_t v = 0; if (!dec_i64(p, e, v)) return false; out = (T) v; return true; }
    if (t == 12) { int64_t v = 0; if (!dec_i64(p, e, v)) return false; out = (T) (uint64_t) v; return true; }
    return false;
}
inline bool bin_dec(const uint8_t *&p, const uint8_t *e, std::string &out) {
    uint8_t t = 0;
    uint32_t n = 0;
    if (!dec_u8(p, e, t) || t != 5 || !dec_u32(p, e, n) || (size_t) (e - p) < n) return false;
    out.assign((const char *) p, n);
    p += n;
    return true;
}
template <class A, class B> bool bin_dec(const uint8_t *&p, const uint8_t *e, std::pair<A, B> &out);
template <class T, class Al> bool bin_dec(const uint8_t *&p, const uint8_t *e, std::vector<T, Al> &out);
template <class T, size_t N> bool bin_dec(const uint8_t *&p, const uint8_t *e, std::array<T, N> &out);
template <class T, class C, class Al> bool bin_dec(const uint8_t *&p, const uint8_t *e, std::map<std::string, T, C, Al> &out);
template <class T, class H, class E, class Al> bool bin_dec(const uint8_t *&p, const uint8_t *e, std::unordered_map<std::string, T, H, E, Al> &out);

template <class A, class B>
bool bin_dec(const uint8_t *&p, const uint8_t *e, std::pair<A, B> &out) {
    uint8_t t = 0;
    uint32_t n = 0;
    if (!dec_u8(p, e, t) || t != 6 || !dec_u32(p, e, n) || n != 2) return false;
    return bin_dec(p, e, out.first) && bin_dec(p, e, out.second);
}
template <class T, class Al, class V>
inline typename std::enable_if<std::is_arithmetic<T>::value, bool>::type
bin_push_num(std::vector<T, Al> &out, V x) {
    out.push_back((T) x);
    return true;
}
template <class T, class Al, class V>
inline typename std::enable_if<!std::is_arithmetic<T>::value, bool>::type
bin_push_num(std::vector<T, Al> &, V) {
    return false;
}
template <class T, class Al>
bool bin_dec(const uint8_t *&p, const uint8_t *e, std::vector<T, Al> &out) {
    uint8_t t = 0;
    uint32_t n = 0;
    if (!dec_u8(p, e, t)) return false;
    out.clear();
    if (t == 0) return true;
    if (!dec_u32(p, e, n)) return false;
    out.reserve(n);
    if (t == 6) {
        for (uint32_t i = 0; i < n; i++) {
            T v{};
            if (!bin_dec(p, e, v)) return false;
            out.push_back(std::move(v));
        }
        return true;
    }
    if (t == 8) {
        for (uint32_t i = 0; i < n; i++) {
            int32_t x = 0;
            if (!dec_i32(p, e, x) || !bin_push_num(out, x)) return false;
        }
        return true;
    }
    if (t == 9) {
        for (uint32_t i = 0; i < n; i++) {
            int64_t x = 0;
            if (!dec_i64(p, e, x) || !bin_push_num(out, x)) return false;
        }
        return true;
    }
    if (t == 10) {
        for (uint32_t i = 0; i < n; i++) {
            double x = 0;
            if (!dec_f64(p, e, x) || !bin_push_num(out, x)) return false;
        }
        return true;
    }
    return false;
}
template <class Al>
inline bool bin_dec(const uint8_t *&p, const uint8_t *e, std::vector<bool, Al> &out) {
    uint8_t t = 0;
    uint32_t n = 0;
    if (!dec_u8(p, e, t)) return false;
    out.clear();
    if (t == 0) return true;
    if (t != 6 || !dec_u32(p, e, n)) return false;
    out.reserve(n);
    for (uint32_t i = 0; i < n; i++) {
        bool v = false;
        if (!bin_dec(p, e, v)) return false;
        out.push_back(v);
    }
    return true;
}
template <class T, size_t N>
bool bin_dec(const uint8_t *&p, const uint8_t *e, std::array<T, N> &out) {
    uint8_t t = 0;
    uint32_t n = 0;
    if (!dec_u8(p, e, t) || t != 6 || !dec_u32(p, e, n)) return false;
    for (size_t i = 0; i < N && i < n; i++) if (!bin_dec(p, e, out[i])) return false;
    return true;
}
template <class M>
bool bin_dec_map(const uint8_t *&p, const uint8_t *e, M &out) {
    uint8_t t = 0;
    uint32_t n = 0;
    if (!dec_u8(p, e, t)) return false;
    out.clear();
    if (t == 0) return true;
    if (t != 7 || !dec_u32(p, e, n)) return false;
    for (uint32_t i = 0; i < n; i++) {
        uint32_t kl = 0;
        if (!dec_u32(p, e, kl) || (size_t) (e - p) < kl) return false;
        std::string k((const char *) p, kl);
        p += kl;
        typename M::mapped_type val{};
        if (!bin_dec(p, e, val)) return false;
        out.emplace(std::move(k), std::move(val));
    }
    return true;
}
template <class T, class C, class Al>
bool bin_dec(const uint8_t *&p, const uint8_t *e, std::map<std::string, T, C, Al> &out) { return bin_dec_map(p, e, out); }
template <class T, class H, class E, class Al>
bool bin_dec(const uint8_t *&p, const uint8_t *e, std::unordered_map<std::string, T, H, E, Al> &out) { return bin_dec_map(p, e, out); }

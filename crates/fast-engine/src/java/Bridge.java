/*PACKAGE*/
/*IMPORTS*/
// fast-n-rust: calls the verified Rust port of /*ID*/. Generated; remove with  fnr disable /*ID*/
// FNR_DISABLE=1 (or FAST_ANYTHING_DISABLE=1) runs the Java version instead. Add --enable-native-access=ALL-UNNAMED to silence Java's notice.
final class /*CLASS*/ {
    static /*RET*/ type; // the original's return type, read back with reflection
    static final java.lang.reflect.Type TYPE;
    static final java.lang.invoke.MethodHandle CALL, BIN, FREE;
    static final boolean ON, BIN_ON;
/*DIRECT_FIELDS*/

    static {
        java.lang.reflect.Type t = Object.class;
        java.lang.invoke.MethodHandle c = null, b = null, f = null;
/*DIRECT_INIT*/
        if (System.getenv("FNR_DISABLE") == null && System.getenv("FAST_ANYTHING_DISABLE") == null) {
            try {
                t = /*CLASS*/.class.getDeclaredField("type").getGenericType();
                var linker = java.lang.foreign.Linker.nativeLinker();
                String envDir = System.getenv("FNR_LIB_DIR");
                java.nio.file.Path libPath = null;
                if (envDir != null && !envDir.isEmpty()) {
                    libPath = java.nio.file.Path.of(envDir, /*LIB_NAME*/);
                } else {
                    for (String cand : new String[] { /*LIB*/, /*LIB_REL*/ }) {
                        java.nio.file.Path p = java.nio.file.Path.of(cand);
                        if (java.nio.file.Files.exists(p)) {
                            libPath = p;
                            break;
                        }
                    }
                    if (libPath == null) {
                        libPath = java.nio.file.Path.of(/*LIB*/);
                    }
                }
                var lib = java.lang.foreign.SymbolLookup.libraryLookup(libPath, java.lang.foreign.Arena.global());
                c = linker.downcallHandle(lib.find("fa_call").orElseThrow(), java.lang.foreign.FunctionDescriptor.of(
                    java.lang.foreign.ValueLayout.ADDRESS, java.lang.foreign.ValueLayout.ADDRESS, java.lang.foreign.ValueLayout.JAVA_LONG, java.lang.foreign.ValueLayout.ADDRESS));
                var bSym = lib.find("fa_bin");
                if (bSym.isPresent()) {
                    b = linker.downcallHandle(bSym.get(), java.lang.foreign.FunctionDescriptor.of(
                        java.lang.foreign.ValueLayout.ADDRESS, java.lang.foreign.ValueLayout.ADDRESS, java.lang.foreign.ValueLayout.JAVA_LONG, java.lang.foreign.ValueLayout.ADDRESS));
                }
                f = linker.downcallHandle(lib.find("fa_free").orElseThrow(), java.lang.foreign.FunctionDescriptor.ofVoid(
                    java.lang.foreign.ValueLayout.ADDRESS, java.lang.foreign.ValueLayout.JAVA_LONG));
/*DIRECT_LOAD*/
            } catch (Throwable e) {
                c = null;
                b = null;
                String strict = System.getenv("FNR_STRICT");
                if ("1".equals(strict) || "true".equalsIgnoreCase(strict)) {
                    throw new ExceptionInInitializerError("fnr: FNR_STRICT=1 and Rust port /*ID*/ (" + /*LIB_NAME*/ + ") could not be loaded: " + e);
                }
            }
        }
        TYPE = t;
        CALL = c;
        BIN = b;
        FREE = f;
        ON = c != null;
        BIN_ON = c != null && b != null;
/*DIRECT_FINALLY*/
    }

/*DIRECT_METHODS*/
    static Object callBin(Object[] args) {
        byte[] in = Bin.encArgs(args);
        try (var arena = java.lang.foreign.Arena.ofConfined()) {
            var input = arena.allocate(Math.max(1, in.length));
            if (in.length > 0) java.lang.foreign.MemorySegment.copy(in, 0, input, java.lang.foreign.ValueLayout.JAVA_BYTE, 0, in.length);
            var len = arena.allocate(java.lang.foreign.ValueLayout.JAVA_LONG);
            var out = (java.lang.foreign.MemorySegment) BIN.invokeExact(input, (long) in.length, len);
            long n = len.get(java.lang.foreign.ValueLayout.JAVA_LONG, 0);
            byte[] bytes = out.reinterpret(n).toArray(java.lang.foreign.ValueLayout.JAVA_BYTE);
            FREE.invokeExact(out, n);
            if (bytes.length == 0) throw new RuntimeException("fnr: empty binary reply");
            if (bytes[0] == 0) {
                int mlen = bytes.length >= 5 ? Bin.i32(bytes, 1) : 0;
                String msg = bytes.length >= 5 + mlen ? new String(bytes, 5, mlen, java.nio.charset.StandardCharsets.UTF_8) : "binary call failed";
                throw new RuntimeException("fnr: " + msg);
            }
            return Bin.decTyped(bytes, new int[] { 1 }, TYPE);
        } catch (RuntimeException | Error e) {
            throw e;
        } catch (Throwable e) {
            throw new RuntimeException(e);
        }
    }

    static Object call(Object[] args) {
        if (BIN_ON) {
            try {
                return callBin(args);
            } catch (Bin.Unsupported ignored) {}
        }
        byte[] in = Json.enc(args).getBytes(java.nio.charset.StandardCharsets.UTF_8);
        try (var arena = java.lang.foreign.Arena.ofConfined()) {
            var input = arena.allocate(Math.max(1, in.length));
            java.lang.foreign.MemorySegment.copy(in, 0, input, java.lang.foreign.ValueLayout.JAVA_BYTE, 0, in.length);
            var len = arena.allocate(java.lang.foreign.ValueLayout.JAVA_LONG);
            var out = (java.lang.foreign.MemorySegment) CALL.invokeExact(input, (long) in.length, len);
            long n = len.get(java.lang.foreign.ValueLayout.JAVA_LONG, 0);
            byte[] bytes = out.reinterpret(n).toArray(java.lang.foreign.ValueLayout.JAVA_BYTE);
            FREE.invokeExact(out, n);
            var reply = (java.util.Map<?, ?>) Json.parse(new String(bytes, java.nio.charset.StandardCharsets.UTF_8));
            if (reply.containsKey("error")) throw new RuntimeException("fnr: " + reply.get("error"));
            return Json.convert(reply.get("ok"), TYPE);
        } catch (RuntimeException | Error e) {
            throw e;
        } catch (Throwable e) {
            throw new RuntimeException(e);
        }
    }

    static final class Bin {
        static final class Unsupported extends RuntimeException {
            Unsupported(String m) { super(m, null, false, false); }
        }
        static final class Buf {
            byte[] b = new byte[256];
            int n = 0;
            void ensure(int extra) {
                if (n + extra > b.length) b = java.util.Arrays.copyOf(b, Math.max(b.length * 2, n + extra));
            }
            void u8(int v) { ensure(1); b[n++] = (byte) v; }
            void i32(int v) {
                ensure(4);
                b[n++] = (byte) v; b[n++] = (byte) (v >> 8); b[n++] = (byte) (v >> 16); b[n++] = (byte) (v >> 24);
            }
            void i64(long v) {
                ensure(8);
                for (int k = 0; k < 8; k++) b[n++] = (byte) (v >> (k * 8));
            }
            void bytes(byte[] src) { ensure(src.length); System.arraycopy(src, 0, b, n, src.length); n += src.length; }
            byte[] toByteArray() { return java.util.Arrays.copyOf(b, n); }
        }
        static byte[] encArgs(Object[] args) {
            Buf w = new Buf();
            w.u8(6);
            w.i32(args.length);
            for (Object a : args) enc(a, w);
            return w.toByteArray();
        }
        static void enc(Object v, Buf w) {
            if (v == null) { w.u8(0); return; }
            if (v instanceof Boolean b) { w.u8(b ? 2 : 1); return; }
            if (v instanceof Integer i) { w.u8(11); w.i32(i); return; }
            if (v instanceof Short || v instanceof Byte) { w.u8(11); w.i32(((Number) v).intValue()); return; }
            if (v instanceof Long l) { w.u8(3); w.i64(l); return; }
            if (v instanceof Double || v instanceof Float) {
                double d = ((Number) v).doubleValue();
                if (!Double.isFinite(d)) throw new Unsupported("NaN or Infinity");
                w.u8(4);
                w.i64(Double.doubleToRawLongBits(d));
                return;
            }
            if (v instanceof String s) {
                byte[] utf = s.getBytes(java.nio.charset.StandardCharsets.UTF_8);
                w.u8(5); w.i32(utf.length); w.bytes(utf);
                return;
            }
            if (v instanceof int[] a) {
                w.u8(8); w.i32(a.length);
                for (int x : a) w.i32(x);
                return;
            }
            if (v instanceof long[] a) {
                w.u8(9); w.i32(a.length);
                for (long x : a) w.i64(x);
                return;
            }
            if (v instanceof double[] a) {
                w.u8(10); w.i32(a.length);
                for (double x : a) {
                    if (!Double.isFinite(x)) throw new Unsupported("NaN or Infinity");
                    w.i64(Double.doubleToRawLongBits(x));
                }
                return;
            }
            if (v instanceof String[] a) {
                w.u8(6); w.i32(a.length);
                for (String s : a) {
                    if (s == null) { w.u8(0); }
                    else { byte[] utf = s.getBytes(java.nio.charset.StandardCharsets.UTF_8); w.u8(5); w.i32(utf.length); w.bytes(utf); }
                }
                return;
            }
            if (v.getClass().isArray()) {
                int len = java.lang.reflect.Array.getLength(v);
                w.u8(6); w.i32(len);
                for (int i = 0; i < len; i++) enc(java.lang.reflect.Array.get(v, i), w);
                return;
            }
            if (v instanceof java.util.List<?> l) {
                w.u8(6); w.i32(l.size());
                for (Object x : l) enc(x, w);
                return;
            }
            if (v instanceof java.util.Map<?, ?> m) {
                w.u8(7); w.i32(m.size());
                for (var e : m.entrySet()) {
                    if (!(e.getKey() instanceof String k)) throw new Unsupported("Map with non-String keys");
                    byte[] ku = k.getBytes(java.nio.charset.StandardCharsets.UTF_8);
                    w.i32(ku.length); w.bytes(ku);
                    enc(e.getValue(), w);
                }
                return;
            }
            throw new Unsupported(v.getClass().getSimpleName());
        }
        static int i32(byte[] b, int p) {
            return (b[p] & 0xFF) | ((b[p + 1] & 0xFF) << 8) | ((b[p + 2] & 0xFF) << 16) | ((b[p + 3] & 0xFF) << 24);
        }
        static long i64(byte[] b, int p) {
            long r = 0;
            for (int k = 0; k < 8; k++) r |= ((long) (b[p + k] & 0xFF)) << (k * 8);
            return r;
        }
        static int readInt(byte[] b, int[] pos) {
            int tag = b[pos[0]++] & 0xFF;
            if (tag == 11) { int v = i32(b, pos[0]); pos[0] += 4; return v; }
            if (tag == 3) { long v = i64(b, pos[0]); pos[0] += 8; return (int) v; }
            if (tag == 4) { double d = Double.longBitsToDouble(i64(b, pos[0])); pos[0] += 8; return (int) d; }
            throw new RuntimeException("fnr: expected int tag, got " + tag);
        }
        static long readLong(byte[] b, int[] pos) {
            int tag = b[pos[0]++] & 0xFF;
            if (tag == 3) { long v = i64(b, pos[0]); pos[0] += 8; return v; }
            if (tag == 11) { int v = i32(b, pos[0]); pos[0] += 4; return v; }
            if (tag == 4) { double d = Double.longBitsToDouble(i64(b, pos[0])); pos[0] += 8; return (long) d; }
            throw new RuntimeException("fnr: expected long tag, got " + tag);
        }
        static double readDouble(byte[] b, int[] pos) {
            int tag = b[pos[0]++] & 0xFF;
            if (tag == 4) { long bits = i64(b, pos[0]); pos[0] += 8; return Double.longBitsToDouble(bits); }
            if (tag == 11) { int v = i32(b, pos[0]); pos[0] += 4; return v; }
            if (tag == 3) { long v = i64(b, pos[0]); pos[0] += 8; return v; }
            throw new RuntimeException("fnr: expected double tag, got " + tag);
        }
        static Object decTyped(byte[] b, int[] pos, java.lang.reflect.Type t) {
            int peek = b[pos[0]] & 0xFF;
            if (peek == 0) { pos[0]++; return null; }
            if (t == int.class || t == Integer.class) return readInt(b, pos);
            if (t == long.class || t == Long.class) return readLong(b, pos);
            if (t == double.class || t == Double.class) return readDouble(b, pos);
            if (t == float.class || t == Float.class) return (float) readDouble(b, pos);
            if (t == boolean.class || t == Boolean.class) { int tag = b[pos[0]++] & 0xFF; return tag == 2; }
            if (t == String.class) { return decVal(b, pos); }
            if (t == int[].class) {
                int tag = b[pos[0]++] & 0xFF;
                int len = i32(b, pos[0]); pos[0] += 4;
                int[] out = new int[len];
                if (tag == 8) { for (int k = 0; k < len; k++) { out[k] = i32(b, pos[0]); pos[0] += 4; } }
                else { for (int k = 0; k < len; k++) out[k] = readInt(b, pos); }
                return out;
            }
            if (t == int[][].class) {
                int tag = b[pos[0]++] & 0xFF;
                int rows = i32(b, pos[0]); pos[0] += 4;
                int[][] out = new int[rows][];
                for (int r = 0; r < rows; r++) out[r] = (int[]) decTyped(b, pos, int[].class);
                return out;
            }
            if (t == long[].class) {
                int tag = b[pos[0]++] & 0xFF;
                int len = i32(b, pos[0]); pos[0] += 4;
                long[] out = new long[len];
                if (tag == 9) { for (int k = 0; k < len; k++) { out[k] = i64(b, pos[0]); pos[0] += 8; } }
                else { for (int k = 0; k < len; k++) out[k] = readLong(b, pos); }
                return out;
            }
            if (t == double[].class) {
                int tag = b[pos[0]++] & 0xFF;
                int len = i32(b, pos[0]); pos[0] += 4;
                double[] out = new double[len];
                if (tag == 10) { for (int k = 0; k < len; k++) { out[k] = Double.longBitsToDouble(i64(b, pos[0])); pos[0] += 8; } }
                else { for (int k = 0; k < len; k++) out[k] = readDouble(b, pos); }
                return out;
            }
            if (t == String[].class) {
                pos[0]++;
                int len = i32(b, pos[0]); pos[0] += 4;
                String[] out = new String[len];
                for (int k = 0; k < len; k++) out[k] = (String) decVal(b, pos);
                return out;
            }
            return Json.convert(decVal(b, pos), t);
        }
        static Object decVal(byte[] b, int[] pos) {
            int tag = b[pos[0]++] & 0xFF;
            switch (tag) {
                case 0: return null;
                case 1: return Boolean.FALSE;
                case 2: return Boolean.TRUE;
                case 3: { long v = i64(b, pos[0]); pos[0] += 8; return v; }
                case 4: { double d = Double.longBitsToDouble(i64(b, pos[0])); pos[0] += 8; return d; }
                case 5: {
                    int n = i32(b, pos[0]); pos[0] += 4;
                    String s = new String(b, pos[0], n, java.nio.charset.StandardCharsets.UTF_8);
                    pos[0] += n;
                    return s;
                }
                case 6: {
                    int n = i32(b, pos[0]); pos[0] += 4;
                    var l = new java.util.ArrayList<Object>(n);
                    for (int k = 0; k < n; k++) l.add(decVal(b, pos));
                    return l;
                }
                case 7: {
                    int n = i32(b, pos[0]); pos[0] += 4;
                    var m = new java.util.LinkedHashMap<String, Object>(n);
                    for (int k = 0; k < n; k++) {
                        int kl = i32(b, pos[0]); pos[0] += 4;
                        String key = new String(b, pos[0], kl, java.nio.charset.StandardCharsets.UTF_8);
                        pos[0] += kl;
                        m.put(key, decVal(b, pos));
                    }
                    return m;
                }
                case 8: {
                    int n = i32(b, pos[0]); pos[0] += 4;
                    var l = new java.util.ArrayList<Object>(n);
                    for (int k = 0; k < n; k++) { l.add((long) i32(b, pos[0])); pos[0] += 4; }
                    return l;
                }
                case 9: {
                    int n = i32(b, pos[0]); pos[0] += 4;
                    var l = new java.util.ArrayList<Object>(n);
                    for (int k = 0; k < n; k++) { l.add(i64(b, pos[0])); pos[0] += 8; }
                    return l;
                }
                case 10: {
                    int n = i32(b, pos[0]); pos[0] += 4;
                    var l = new java.util.ArrayList<Object>(n);
                    for (int k = 0; k < n; k++) { l.add(Double.longBitsToDouble(i64(b, pos[0]))); pos[0] += 8; }
                    return l;
                }
                case 11: { int v = i32(b, pos[0]); pos[0] += 4; return (long) v; }
                case 12: { long v = i64(b, pos[0]); pos[0] += 8; return v; }
                default: throw new RuntimeException("fnr: unknown wire tag " + tag);
            }
        }
    }

/*JSON*/
}

// fast-n-rust: calls the verified Rust port of /*ID*/. Generated; remove with  fnr disable /*ID*/
// FNR_DISABLE=1 (or FAST_ANYTHING_DISABLE=1) runs the C# version instead.
#nullable enable
#pragma warning disable
using System;
using System.Buffers.Binary;
using System.Collections;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;
using System.Runtime.InteropServices;

static class /*CLASS*/
{
    public static readonly bool On;
    public static readonly bool BinOn;

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    delegate IntPtr FaCall(byte[] input, UIntPtr len, out UIntPtr outLen);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    delegate void FaFree(IntPtr ptr, UIntPtr len);

    static readonly FaCall? CallFn;
    static readonly FaCall? BinFn;
    static readonly FaFree? FreeFn;
/*DIRECT_DECL*/

    static /*CLASS*/()
    {
        if (Environment.GetEnvironmentVariable("FNR_DISABLE") != null || Environment.GetEnvironmentVariable("FAST_ANYTHING_DISABLE") != null) return;
        try
        {
            var envDir = Environment.GetEnvironmentVariable("FNR_LIB_DIR");
            string? libPath = !string.IsNullOrEmpty(envDir)
                ? Path.Combine(envDir, /*LIB_NAME*/)
                : null;
            if (libPath == null)
            {
                foreach (var cand in new[]
                {
                    Path.GetFullPath(/*LIB*/),
                    Path.GetFullPath(/*LIB_REL*/),
                    Path.Combine(AppContext.BaseDirectory, ".fast", "lib", /*LIB_NAME*/),
                })
                {
                    if (File.Exists(cand))
                    {
                        libPath = cand;
                        break;
                    }
                }
                libPath ??= Path.GetFullPath(/*LIB*/);
            }
            var lib = NativeLibrary.Load(libPath);
            CallFn = Marshal.GetDelegateForFunctionPointer<FaCall>(NativeLibrary.GetExport(lib, "fa_call"));
            FreeFn = Marshal.GetDelegateForFunctionPointer<FaFree>(NativeLibrary.GetExport(lib, "fa_free"));
            if (NativeLibrary.TryGetExport(lib, "fa_bin", out var binPtr))
            {
                BinFn = Marshal.GetDelegateForFunctionPointer<FaCall>(binPtr);
                BinOn = true;
            }
/*DIRECT_LOAD*/
            On = true;
        }
        catch (Exception ex)
        {
            On = false;
            BinOn = false;
            var strict = Environment.GetEnvironmentVariable("FNR_STRICT");
            if (strict == "1" || string.Equals(strict, "true", StringComparison.OrdinalIgnoreCase))
            {
                throw new InvalidOperationException("fnr: FNR_STRICT=1 and Rust port /*ID*/ (" + /*LIB_NAME*/ + ") could not be loaded", ex);
            }
        }
    }

/*DIRECT_METHODS*/
    public static object? CallBin(object?[] args, Type type)
    {
        byte[] input = Bin.EncArgs(args);
        IntPtr p = BinFn!(input, (UIntPtr)input.Length, out UIntPtr n);
        var reply = new byte[(int)n];
        Marshal.Copy(p, reply, 0, reply.Length);
        FreeFn!(p, n);
        if (reply.Length == 0) throw new InvalidOperationException("fnr: empty binary reply");
        if (reply[0] == 0)
        {
            int mlen = reply.Length >= 5 ? BinaryPrimitives.ReadInt32LittleEndian(reply.AsSpan(1, 4)) : 0;
            string msg = reply.Length >= 5 + mlen ? Encoding.UTF8.GetString(reply, 5, mlen) : "binary call failed";
            throw new InvalidOperationException("fnr: " + msg);
        }
        int pos = 1;
        return Bin.Dec(reply, ref pos, type);
    }

    public static object? Call(object?[] args, Type type)
    {
        if (BinOn)
        {
            try
            {
                return CallBin(args, type);
            }
            catch (Bin.Unsupported) { }
        }
        byte[] input = Encoding.UTF8.GetBytes(Json.Enc(args));
        IntPtr p = CallFn!(input, (UIntPtr)input.Length, out UIntPtr n);
        var reply = new byte[(int)n];
        Marshal.Copy(p, reply, 0, reply.Length);
        FreeFn!(p, n);
        using var doc = JsonDocument.Parse(reply);
        if (doc.RootElement.TryGetProperty("error", out var e)) throw new InvalidOperationException("fnr: " + e);
        return JsonSerializer.Deserialize(doc.RootElement.GetProperty("ok"), type, Json.Options);
    }

    internal static class Bin
    {
        internal sealed class Unsupported : Exception
        {
            public Unsupported(string m) : base(m) { }
        }

        sealed class Buf
        {
            byte[] _b = new byte[256];
            int _n = 0;
            void Ensure(int extra)
            {
                if (_n + extra > _b.Length)
                {
                    int next = Math.Max(_b.Length * 2, _n + extra);
                    Array.Resize(ref _b, next);
                }
            }
            public void U8(byte v) { Ensure(1); _b[_n++] = v; }
            public void I32(int v) { Ensure(4); BinaryPrimitives.WriteInt32LittleEndian(_b.AsSpan(_n, 4), v); _n += 4; }
            public void I64(long v) { Ensure(8); BinaryPrimitives.WriteInt64LittleEndian(_b.AsSpan(_n, 8), v); _n += 8; }
            public void F64(double v) { Ensure(8); BinaryPrimitives.WriteInt64LittleEndian(_b.AsSpan(_n, 8), BitConverter.DoubleToInt64Bits(v)); _n += 8; }
            public void Str(string s)
            {
                int max = Encoding.UTF8.GetByteCount(s);
                U8(5);
                I32(max);
                Ensure(max);
                Encoding.UTF8.GetBytes(s, _b.AsSpan(_n, max));
                _n += max;
            }
            public void RawStr(string s)
            {
                int max = Encoding.UTF8.GetByteCount(s);
                I32(max);
                Ensure(max);
                Encoding.UTF8.GetBytes(s, _b.AsSpan(_n, max));
                _n += max;
            }
            public byte[] ToArray()
            {
                var r = new byte[_n];
                Buffer.BlockCopy(_b, 0, r, 0, _n);
                return r;
            }
        }

        public static byte[] EncArgs(object?[] args)
        {
            var w = new Buf();
            w.U8(6);
            w.I32(args.Length);
            foreach (var a in args) Enc(a, w);
            return w.ToArray();
        }

        static void Enc(object? v, Buf w)
        {
            if (v is null) { w.U8(0); return; }
            if (v is bool b) { w.U8((byte)(b ? 2 : 1)); return; }
            if (v is int i32) { w.U8(11); w.I32(i32); return; }
            if (v is sbyte or byte or short or ushort) { w.U8(11); w.I32(Convert.ToInt32(v)); return; }
            if (v is long or uint) { w.U8(3); w.I64(Convert.ToInt64(v)); return; }
            if (v is ulong ul) { w.U8(12); w.I64(unchecked((long)ul)); return; }
            if (v is float f) { if (!float.IsFinite(f)) throw new Unsupported("NaN/Inf"); w.U8(4); w.F64(f); return; }
            if (v is double d) { if (!double.IsFinite(d)) throw new Unsupported("NaN/Inf"); w.U8(4); w.F64(d); return; }
            if (v is string s) { w.Str(s); return; }
            if (v is char ch) { w.Str(ch.ToString()); return; }
            if (v is int[] ia) { w.U8(8); w.I32(ia.Length); foreach (var x in ia) w.I32(x); return; }
            if (v is long[] la) { w.U8(9); w.I32(la.Length); foreach (var x in la) w.I64(x); return; }
            if (v is double[] da) { w.U8(10); w.I32(da.Length); foreach (var x in da) { if (!double.IsFinite(x)) throw new Unsupported("NaN/Inf"); w.F64(x); } return; }
            if (v is string[] sa) { w.U8(6); w.I32(sa.Length); foreach (var x in sa) { if (x is null) w.U8(0); else w.Str(x); } return; }
            if (v is IDictionary dict)
            {
                w.U8(7);
                w.I32(dict.Count);
                foreach (DictionaryEntry e in dict)
                {
                    if (e.Key is not string k) throw new Unsupported("non-string map key");
                    w.RawStr(k);
                    Enc(e.Value, w);
                }
                return;
            }
            if (v is ICollection col)
            {
                w.U8(6);
                w.I32(col.Count);
                foreach (var x in col) Enc(x, w);
                return;
            }
            if (v is IEnumerable en)
            {
                var items = new List<object?>();
                foreach (var x in en) items.Add(x);
                w.U8(6);
                w.I32(items.Count);
                foreach (var x in items) Enc(x, w);
                return;
            }
            throw new Unsupported(v.GetType().Name);
        }

        static int ReadI32(byte[] b, ref int p) { int v = BinaryPrimitives.ReadInt32LittleEndian(b.AsSpan(p, 4)); p += 4; return v; }
        static long ReadI64(byte[] b, ref int p) { long v = BinaryPrimitives.ReadInt64LittleEndian(b.AsSpan(p, 8)); p += 8; return v; }
        static double ReadF64(byte[] b, ref int p) { double v = BitConverter.Int64BitsToDouble(ReadI64(b, ref p)); return v; }
        static string ReadStrBody(byte[] b, ref int p) { int n = ReadI32(b, ref p); string s = Encoding.UTF8.GetString(b, p, n); p += n; return s; }

        static long ReadNumAsInt(byte[] b, ref int p)
        {
            byte t = b[p++];
            return t switch
            {
                11 => ReadI32(b, ref p),
                3 => ReadI64(b, ref p),
                12 => ReadI64(b, ref p),
                4 => (long)ReadF64(b, ref p),
                _ => throw new InvalidOperationException($"fnr: expected int tag, got {t}"),
            };
        }

        static double ReadNumAsDouble(byte[] b, ref int p)
        {
            byte t = b[p++];
            return t switch
            {
                4 => ReadF64(b, ref p),
                11 => ReadI32(b, ref p),
                3 => ReadI64(b, ref p),
                12 => (double)unchecked((ulong)ReadI64(b, ref p)),
                _ => throw new InvalidOperationException($"fnr: expected float tag, got {t}"),
            };
        }

        public static object? Dec(byte[] b, ref int p, Type t)
        {
            byte peek = b[p];
            if (peek == 0) { p++; return null; }
            var under = Nullable.GetUnderlyingType(t);
            if (under != null) t = under;
            if (t == typeof(bool)) { return b[p++] == 2; }
            if (t == typeof(int)) return (int)ReadNumAsInt(b, ref p);
            if (t == typeof(long)) return ReadNumAsInt(b, ref p);
            if (t == typeof(short)) return (short)ReadNumAsInt(b, ref p);
            if (t == typeof(sbyte)) return (sbyte)ReadNumAsInt(b, ref p);
            if (t == typeof(byte)) return (byte)ReadNumAsInt(b, ref p);
            if (t == typeof(ushort)) return (ushort)ReadNumAsInt(b, ref p);
            if (t == typeof(uint)) return (uint)ReadNumAsInt(b, ref p);
            if (t == typeof(ulong)) return unchecked((ulong)ReadNumAsInt(b, ref p));
            if (t == typeof(double)) return ReadNumAsDouble(b, ref p);
            if (t == typeof(float)) return (float)ReadNumAsDouble(b, ref p);
            if (t == typeof(string)) { if (b[p++] != 5) throw new InvalidOperationException("expected string"); return ReadStrBody(b, ref p); }
            if (t == typeof(char)) { if (b[p++] != 5) throw new InvalidOperationException("expected char string"); var s = ReadStrBody(b, ref p); return s.Length > 0 ? s[0] : '\0'; }
            if (t == typeof(int[]))
            {
                byte tag = b[p++]; int n = ReadI32(b, ref p);
                var arr = new int[n];
                if (tag == 8) { for (int k = 0; k < n; k++) arr[k] = ReadI32(b, ref p); }
                else { for (int k = 0; k < n; k++) arr[k] = (int)ReadNumAsInt(b, ref p); }
                return arr;
            }
            if (t == typeof(int[][]))
            {
                p++;
                int rows = ReadI32(b, ref p);
                var outer = new int[rows][];
                for (int r = 0; r < rows; r++)
                {
                    outer[r] = (int[])Dec(b, ref p, typeof(int[]))!;
                }
                return outer;
            }
            if (t == typeof(long[]))
            {
                byte tag = b[p++]; int n = ReadI32(b, ref p);
                var arr = new long[n];
                if (tag == 9) { for (int k = 0; k < n; k++) arr[k] = ReadI64(b, ref p); }
                else { for (int k = 0; k < n; k++) arr[k] = ReadNumAsInt(b, ref p); }
                return arr;
            }
            if (t == typeof(double[]))
            {
                byte tag = b[p++]; int n = ReadI32(b, ref p);
                var arr = new double[n];
                if (tag == 10) { for (int k = 0; k < n; k++) arr[k] = ReadF64(b, ref p); }
                else { for (int k = 0; k < n; k++) arr[k] = ReadNumAsDouble(b, ref p); }
                return arr;
            }
            if (t.IsArray)
            {
                var elem = t.GetElementType()!;
                byte tag = b[p++];
                int n = ReadI32(b, ref p);
                var arr = Array.CreateInstance(elem, n);
                if (tag == 8) { for (int k = 0; k < n; k++) arr.SetValue(Convert.ChangeType(ReadI32(b, ref p), elem), k); }
                else if (tag == 9) { for (int k = 0; k < n; k++) arr.SetValue(Convert.ChangeType(ReadI64(b, ref p), elem), k); }
                else if (tag == 10) { for (int k = 0; k < n; k++) arr.SetValue(Convert.ChangeType(ReadF64(b, ref p), elem), k); }
                else { for (int k = 0; k < n; k++) arr.SetValue(Dec(b, ref p, elem), k); }
                return arr;
            }
            if (t.IsGenericType && t.GetGenericTypeDefinition() == typeof(List<>))
            {
                var elem = t.GetGenericArguments()[0];
                byte tag = b[p++];
                int n = ReadI32(b, ref p);
                var list = (IList)Activator.CreateInstance(t, n)!;
                if (tag == 8) { for (int k = 0; k < n; k++) list.Add(Convert.ChangeType(ReadI32(b, ref p), elem)); }
                else if (tag == 9) { for (int k = 0; k < n; k++) list.Add(Convert.ChangeType(ReadI64(b, ref p), elem)); }
                else if (tag == 10) { for (int k = 0; k < n; k++) list.Add(Convert.ChangeType(ReadF64(b, ref p), elem)); }
                else { for (int k = 0; k < n; k++) list.Add(Dec(b, ref p, elem)); }
                return list;
            }
            if (t.IsGenericType && t.GetGenericTypeDefinition() == typeof(Dictionary<,>))
            {
                var valT = t.GetGenericArguments()[1];
                if (b[p++] != 7) throw new InvalidOperationException("expected map tag");
                int n = ReadI32(b, ref p);
                var dict = (IDictionary)Activator.CreateInstance(t, n)!;
                for (int k = 0; k < n; k++)
                {
                    var key = ReadStrBody(b, ref p);
                    dict[key] = Dec(b, ref p, valT);
                }
                return dict;
            }
            throw new Unsupported("unsupported decode type " + t.Name);
        }
    }

/*JSON*/
}

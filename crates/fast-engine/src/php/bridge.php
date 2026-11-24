final class /*CLASS*/
{
    // Calls the verified Rust port of /*ID*/ through PHP's FFI extension (extension=ffi in php.ini).
    // FNR_DISABLE=1 (or FAST_ANYTHING_DISABLE=1), or no FFI, runs the PHP version instead.
    public static ?bool $on = null;
    private static $ffi = null;
    private static $binFfi = null;
    private static $directFfi = null;
    private static array $params = [];

    public static function load(): bool
    {
        self::$on = false;
        $fnrEnvOn = static function (string $name): bool {
            $v = getenv($name);
            return $v !== false && $v !== '' && $v !== '0' && strtolower($v) !== 'false';
        };
        if ($fnrEnvOn('FNR_DISABLE') || $fnrEnvOn('FAST_ANYTHING_DISABLE') || !extension_loaded('ffi')) return false;
        $envDir = getenv('FNR_LIB_DIR');
        $lib = ($envDir !== false && $envDir !== '')
            ? rtrim($envDir, "/\\") . '/' . /*LIB_NAME*/
            : __DIR__ . '/' . /*LIB*/;
        try {
            self::$ffi = \FFI::cdef('uint8_t *fa_call(const char *input, size_t len, size_t *out_len); void fa_free(uint8_t *ptr, size_t len);', $lib);
        } catch (\Throwable $e) {
            $strict = getenv('FNR_STRICT');
            if ($strict === '1' || ($strict !== false && strtolower($strict) === 'true')) {
                throw new \RuntimeException('fnr: FNR_STRICT=1 and Rust port /*ID*/ could not be loaded: ' . $e->getMessage(), 0, $e);
            }
            return false;
        }
        try {
            self::$binFfi = \FFI::cdef('uint8_t *fa_bin(const uint8_t *input, size_t len, size_t *out_len);', $lib);
        } catch (\Throwable $e) {
            self::$binFfi = null;
        }
/*DIRECT_LOAD*/
        return self::$on = true;
    }

    private static function encBin($v): ?string
    {
        if ($v === null) return "\x00";
        if (is_bool($v)) return $v ? "\x02" : "\x01";
        if (is_int($v)) {
            if ($v >= -2147483648 && $v <= 2147483647) return "\x0b" . pack('V', $v & 0xFFFFFFFF);
            return "\x03" . pack('P', $v);
        }
        if (is_float($v)) {
            if (!is_finite($v)) return null;
            return "\x04" . pack('e', $v);
        }
        if (is_string($v)) {
            if (!mb_check_encoding($v, 'UTF-8')) return null;
            return "\x05" . pack('V', strlen($v)) . $v;
        }
        if (is_array($v)) {
            if (array_is_list($v)) {
                $out = "\x06" . pack('V', count($v));
                foreach ($v as $item) {
                    $e = self::encBin($item);
                    if ($e === null) return null;
                    $out .= $e;
                }
                return $out;
            }
            $out = "\x07" . pack('V', count($v));
            foreach ($v as $k => $val) {
                $ks = (string)$k;
                $e = self::encBin($val);
                if ($e === null) return null;
                $out .= pack('V', strlen($ks)) . $ks . $e;
            }
            return $out;
        }
        return null;
    }

    private static function decBin(string $b, int &$p)
    {
        $tag = ord($b[$p++]);
        if ($tag === 0) return null;
        if ($tag === 1) return false;
        if ($tag === 2) return true;
        if ($tag === 11) {
            $u = unpack('V', $b, $p)[1];
            $p += 4;
            return $u >= 0x80000000 ? $u - 0x100000000 : $u;
        }
        if ($tag === 3 || $tag === 12) {
            $u = unpack('P', $b, $p)[1];
            $p += 8;
            return $u;
        }
        if ($tag === 4) {
            $f = unpack('e', $b, $p)[1];
            $p += 8;
            return $f;
        }
        if ($tag === 5) {
            $n = unpack('V', $b, $p)[1];
            $p += 4;
            $s = substr($b, $p, $n);
            $p += $n;
            return $s;
        }
        if ($tag === 6) {
            $n = unpack('V', $b, $p)[1];
            $p += 4;
            $arr = [];
            for ($i = 0; $i < $n; $i++) $arr[] = self::decBin($b, $p);
            return $arr;
        }
        if ($tag === 7) {
            $n = unpack('V', $b, $p)[1];
            $p += 4;
            $arr = [];
            for ($i = 0; $i < $n; $i++) {
                $kl = unpack('V', $b, $p)[1];
                $p += 4;
                $k = substr($b, $p, $kl);
                $p += $kl;
                $arr[$k] = self::decBin($b, $p);
            }
            return $arr;
        }
        if ($tag === 8) {
            $n = unpack('V', $b, $p)[1];
            $p += 4;
            $arr = [];
            for ($i = 0; $i < $n; $i++) {
                $u = unpack('V', $b, $p)[1];
                $p += 4;
                $arr[] = $u >= 0x80000000 ? $u - 0x100000000 : $u;
            }
            return $arr;
        }
        if ($tag === 9) {
            $n = unpack('V', $b, $p)[1];
            $p += 4;
            $arr = [];
            for ($i = 0; $i < $n; $i++) {
                $arr[] = unpack('P', $b, $p)[1];
                $p += 8;
            }
            return $arr;
        }
        if ($tag === 10) {
            $n = unpack('V', $b, $p)[1];
            $p += 4;
            $arr = [];
            for ($i = 0; $i < $n; $i++) {
                $arr[] = unpack('e', $b, $p)[1];
                $p += 8;
            }
            return $arr;
        }
        throw new \RuntimeException('fnr: bad binary wire tag ' . $tag);
    }

/*DIRECT_METHOD*/
    public static function call(string $name, array $args)
    {
        $ps = self::$params[$name] ??= (str_contains($name, '::') ? new \ReflectionMethod(...explode('::', $name, 2)) : new \ReflectionFunction($name))->getParameters();
        for ($i = count($args); $i < count($ps) && $ps[$i]->isDefaultValueAvailable(); $i++) $args[] = $ps[$i]->getDefaultValue();
        if (self::$binFfi !== null && ($bin = self::encBin($args)) !== null) {
            $len = self::$ffi->new('size_t');
            $buf = self::$ffi->new('uint8_t[' . max(1, strlen($bin)) . ']');
            if ($bin !== '') \FFI::memcpy($buf, $bin, strlen($bin));
            $out = self::$binFfi->fa_bin($buf, strlen($bin), \FFI::addr($len));
            $n = $len->cdata;
            $text = \FFI::string($out, $n);
            self::$ffi->fa_free($out, $n);
            if ($n > 0 && ord($text[0]) === 0) {
                $ml = $n >= 5 ? unpack('V', $text, 1)[1] : 0;
                throw new \RuntimeException('fnr: ' . ($n >= 5 + $ml ? substr($text, 5, $ml) : 'binary error'));
            }
            if ($n > 0 && ord($text[0]) === 1) {
                $p = 1;
                return self::decBin($text, $p);
            }
        }
        $in = json_encode($args, JSON_PRESERVE_ZERO_FRACTION | JSON_UNESCAPED_UNICODE | JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR);
        $len = self::$ffi->new('size_t');
        $out = self::$ffi->fa_call($in, strlen($in), \FFI::addr($len));
        $n = $len->cdata;
        $text = \FFI::string($out, $n);
        self::$ffi->fa_free($out, $n);
        $reply = json_decode($text, true, 512, JSON_THROW_ON_ERROR);
        if (array_key_exists('error', $reply)) throw new \RuntimeException('fnr: ' . $reply['error']);
        return $reply['ok'];
    }
}

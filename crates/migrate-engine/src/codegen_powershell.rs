//! The PowerShell side of the JSON dispatcher: a wrapper script with the original functions,
//! each calling `rust_anything_call` through P/Invoke methods that Reflection.Emit declares when
//! the wrapper loads (no C# compiler, no `Add-Type`), and the driver `record`, `diff` and `bench`
//! run in `pwsh`.

use std::fmt::Write as _;

use crate::bridge::Manifest;

/// Encodes and decodes values as the other bridges do (`$num` markers for NaN, the infinities
/// and -0), and makes one call into a library. Kept in one script variable so that a module that
/// exports every function does not export these too.
const CODEC_PS1: &str = r#"if ($null -eq $script:RustAnythingCodec) {
    $script:RustAnythingCodec = @{}
    $script:RustAnythingCodec.Options = [System.Text.Json.JsonSerializerOptions]::new()
    $script:RustAnythingCodec.Options.Encoder = [System.Text.Encodings.Web.JavaScriptEncoder]::UnsafeRelaxedJsonEscaping
    $script:RustAnythingCodec.Integers = [System.Collections.Generic.HashSet[type]]::new([type[]]@(
        [int], [long], [int16], [byte], [sbyte], [uint16], [uint32], [uint64], [System.Numerics.BigInteger]))
    $script:RustAnythingCodec.Escaped = '["\\\p{Cc}\p{Cs}]'
    $script:RustAnythingCodec.Scalar = [regex]'^\{"ok":(?:(true|false|null)|(-?\d{1,18})|(-?\d+(?:\.\d+(?:[eE][-+]?\d+)?|[eE][-+]?\d+))|"([^"\\\p{Cc}]*)")\}$'
    $script:RustAnythingCodec.String = {
        param([string]$Text)
        [System.Text.Json.JsonSerializer]::Serialize($Text, [string], $script:RustAnythingCodec.Options)
    }
    $script:RustAnythingCodec.Write = {
        param($Value, [System.Text.StringBuilder]$Out)
        $codec = $script:RustAnythingCodec
        if ($null -eq $Value) {
            [void]$Out.Append('null')
            return
        }
        if ($Value -is [System.Management.Automation.PSObject]) { $b = $Value.psobject.BaseObject } else { $b = $Value }
        if ($b -is [string]) {
            if ($b -cmatch $codec.Escaped) {
                [void]$Out.Append([System.Text.Json.JsonSerializer]::Serialize($b, [string], $codec.Options))
            } else {
                [void]$Out.Append('"').Append($b).Append('"')
            }
        } elseif ($codec.Integers.Contains($b.GetType())) {
            [void]$Out.Append($b.ToString([cultureinfo]::InvariantCulture))
        } elseif ($b -is [char]) {
            [void]$Out.Append((& $codec.String ([string]$b)))
        } elseif ($b -is [bool]) {
            [void]$Out.Append($(if ($b) { 'true' } else { 'false' }))
        } elseif ($b -is [System.Management.Automation.SwitchParameter]) {
            [void]$Out.Append($(if ($b.IsPresent) { 'true' } else { 'false' }))
        } elseif ($b -is [double] -or $b -is [float]) {
            $d = [double]$b
            if ([double]::IsNaN($d)) {
                [void]$Out.Append('{"$num":"NaN"}')
            } elseif ([double]::IsPositiveInfinity($d)) {
                [void]$Out.Append('{"$num":"Infinity"}')
            } elseif ([double]::IsNegativeInfinity($d)) {
                [void]$Out.Append('{"$num":"-Infinity"}')
            } elseif ($d -eq 0 -and [double]::IsNegative($d)) {
                [void]$Out.Append('{"$num":"-0"}')
            } else {
                $s = $d.ToString('R', [cultureinfo]::InvariantCulture)
                if ($s -notmatch '[.eE]') { $s += '.0' }
                [void]$Out.Append($s)
            }
        } elseif ($b -is [decimal]) {
            $s = $b.ToString([cultureinfo]::InvariantCulture)
            if ($s -notmatch '[.eE]') { $s += '.0' }
            [void]$Out.Append($s)
        } elseif ($b -is [enum]) {
            [void]$Out.Append((& $codec.String $b.ToString()))
        } elseif ($b -is [System.Collections.IDictionary]) {
            $entries = [object[]]@($b.GetEnumerator())
            if ($b.GetType() -eq [hashtable]) {
                $names = [string[]]::new($entries.Count)
                for ($i = 0; $i -lt $entries.Count; $i++) { $names[$i] = [string]$entries[$i].Key }
                [Array]::Sort($names, $entries, [StringComparer]::Ordinal)
            }
            [void]$Out.Append('{')
            $first = $true
            foreach ($e in $entries) {
                if (-not $first) { [void]$Out.Append(',') }
                $first = $false
                [void]$Out.Append((& $codec.String ([string]$e.Key))).Append(':')
                & $codec.Write $e.Value $Out
            }
            [void]$Out.Append('}')
        } elseif ($b -is [System.Management.Automation.PSCustomObject]) {
            [void]$Out.Append('{')
            $first = $true
            foreach ($p in $Value.psobject.Properties) {
                if (-not $first) { [void]$Out.Append(',') }
                $first = $false
                [void]$Out.Append((& $codec.String $p.Name)).Append(':')
                & $codec.Write $p.Value $Out
            }
            [void]$Out.Append('}')
        } elseif ($b -is [System.Collections.IEnumerable]) {
            [void]$Out.Append('[')
            $first = $true
            foreach ($x in $b) {
                if (-not $first) { [void]$Out.Append(',') }
                $first = $false
                if ($x -is [string] -and $x -cnotmatch $codec.Escaped) {
                    [void]$Out.Append('"').Append($x).Append('"')
                } elseif ($x -is [int] -or $x -is [long]) {
                    [void]$Out.Append($x.ToString([cultureinfo]::InvariantCulture))
                } elseif ($null -eq $x) {
                    [void]$Out.Append('null')
                } else {
                    & $codec.Write $x $Out
                }
            }
            [void]$Out.Append(']')
        } else {
            [void]$Out.Append('{"$object":').Append((& $codec.String $b.GetType().FullName)).Append('}')
        }
    }
    $script:RustAnythingCodec.Json = {
        param($Value)
        $out = [System.Text.StringBuilder]::new()
        & $script:RustAnythingCodec.Write $Value $out
        $out.ToString()
    }
    $script:RustAnythingCodec.DocumentOptions = [System.Text.Json.JsonDocumentOptions]::new()
    $script:RustAnythingCodec.DocumentOptions.MaxDepth = 100000
    $script:RustAnythingCodec.Element = {
        param([System.Text.Json.JsonElement]$Element)
        switch ($Element.ValueKind) {
            'String' { return $Element.GetString() }
            'Number' {
                $raw = $Element.GetRawText()
                if ($raw -cmatch '^-?\d+$') {
                    $long = 0L
                    if ([long]::TryParse($raw, [System.Globalization.NumberStyles]::AllowLeadingSign, [cultureinfo]::InvariantCulture, [ref]$long)) { return $long }
                    return [System.Numerics.BigInteger]::Parse($raw, [cultureinfo]::InvariantCulture)
                }
                return [double]::Parse($raw, [cultureinfo]::InvariantCulture)
            }
            'True' { return $true }
            'False' { return $false }
            'Array' {
                $list = [System.Collections.Generic.List[object]]::new()
                foreach ($x in $Element.EnumerateArray()) { $list.Add((& $script:RustAnythingCodec.Element $x)) }
                return , $list.ToArray()
            }
            'Object' {
                $map = [System.Management.Automation.OrderedHashtable]::new()
                foreach ($p in $Element.EnumerateObject()) { $map[$p.Name] = & $script:RustAnythingCodec.Element $p.Value }
                return , $map
            }
        }
    }
    $script:RustAnythingCodec.Parse = {
        param([string]$Json)
        $document = [System.Text.Json.JsonDocument]::Parse($Json, $script:RustAnythingCodec.DocumentOptions)
        try {
            $v = & $script:RustAnythingCodec.Element $document.RootElement
        } finally {
            $document.Dispose()
        }
        if ($null -eq $v) { return }
        , $v
    }
    $script:RustAnythingCodec.Unmark = {
        param($Value)
        if ($null -eq $Value) { return }
        if ($Value -is [System.Collections.IDictionary]) {
            if ($Value.Count -eq 1 -and $Value.Contains('$num')) {
                switch ($Value['$num']) {
                    'NaN' { return [double]::NaN }
                    'Infinity' { return [double]::PositiveInfinity }
                    '-Infinity' { return [double]::NegativeInfinity }
                    '-0' { return [BitConverter]::Int64BitsToDouble([long]::MinValue) }
                }
            }
            if ($Value.Count -eq 1 -and $Value.Contains('$undefined')) { return }
            foreach ($k in @($Value.Keys)) { $Value[$k] = & $script:RustAnythingCodec.Unmark $Value[$k] }
            return , $Value
        }
        if ($Value -is [array]) {
            for ($i = 0; $i -lt $Value.Count; $i++) { $Value[$i] = & $script:RustAnythingCodec.Unmark $Value[$i] }
            return , $Value
        }
        $Value
    }
    $script:RustAnythingCodec.Error = {
        param([string]$Message)
        if ($Message -match '(?s)^([A-Za-z_][\w.+]*): (.*)$') {
            $type = $Matches[1] -as [type]
            if ($null -ne $type -and [Exception].IsAssignableFrom($type)) {
                try { return [Activator]::CreateInstance($type, [object[]]@($Matches[2])) } catch { }
            }
        }
        [System.Management.Automation.RuntimeException]::new($Message)
    }
    $script:RustAnythingCodec.Load = {
        param([string]$Path)
        if (-not (Test-Path -LiteralPath $Path)) {
            throw "rust-anything: no built library at $Path (run ``rust-anything build``)"
        }
        $name = 'RustAnything' + [guid]::NewGuid().ToString('N')
        $assembly = [System.Reflection.Emit.AssemblyBuilder]::DefineDynamicAssembly([System.Reflection.AssemblyName]::new($name), 'Run')
        $type = $assembly.DefineDynamicModule($name).DefineType("$name.Native", 'Public,Class,Abstract,Sealed')
        $call = $type.DefinePInvokeMethod('rust_anything_call', $Path, 'Public,Static,PinvokeImpl,HideBySig',
            'Standard', [IntPtr], [type[]]@([byte[]], [byte[]]), 'Cdecl', 'Ansi')
        $call.SetImplementationFlags('PreserveSig')
        $free = $type.DefinePInvokeMethod('rust_anything_free', $Path, 'Public,Static,PinvokeImpl,HideBySig',
            'Standard', [void], [type[]]@([IntPtr]), 'Cdecl', 'Ansi')
        $free.SetImplementationFlags('PreserveSig')
        $type.CreateType()
    }
    $script:RustAnythingCodec.Call = {
        param($Native, [string]$Name, [object[]]$Arguments)
        $codec = $script:RustAnythingCodec
        $utf8 = [System.Text.Encoding]::UTF8
        $json = [System.Text.StringBuilder]::new()
        & $codec.Write $Arguments $json
        $ptr = $Native::rust_anything_call($utf8.GetBytes($Name + [char]0), $utf8.GetBytes($json.Append([char]0).ToString()))
        if ($ptr -eq [IntPtr]::Zero) { throw "rust-anything: no answer from ``$Name``" }
        try {
            $text = [System.Runtime.InteropServices.Marshal]::PtrToStringUTF8($ptr)
        } finally {
            $Native::rust_anything_free($ptr)
        }
        $scalar = $codec.Scalar.Match($text)
        if ($scalar.Success) {
            $g = $scalar.Groups
            if ($g[1].Success) {
                if ($g[1].Value -eq 'true') { return $true }
                if ($g[1].Value -eq 'false') { return $false }
                return
            }
            if ($g[2].Success) { return [long]::Parse($g[2].Value, [cultureinfo]::InvariantCulture) }
            if ($g[3].Success) { return [double]::Parse($g[3].Value, [cultureinfo]::InvariantCulture) }
            return $g[4].Value
        }
        $answer = & $codec.Parse $text
        if ($answer.Contains('panic')) { throw "Rust panic: $($answer['panic'])" }
        if ($answer.Contains('error')) { throw (& $codec.Error ([string]$answer['error'])) }
        $value = $answer['ok']
        if ($text.Contains('"$')) { $value = & $codec.Unmark $value }
        if ($null -eq $value) { return }
        if ($value -is [array] -and $value.Count -ge 2) { return $value }
        if ($value -is [array]) { return , $value }
        $value
    }
}
"#;

pub fn wrapper_path(m: &Manifest) -> String {
    format!("ps/{}.ps1", m.lib_name())
}

fn native_var(m: &Manifest) -> String {
    format!("RustAnything_{}", m.lib_name())
}

pub fn wrapper(m: &Manifest) -> String {
    let lib = m.lib_name();
    let var = native_var(m);
    let mut out = format!(
        "# The Rust port of `{unit}`, generated by rust-anything: the same functions, each calling the\n\
         # Rust library through P/Invoke. Dot-source it after the original functions are defined\n\
         # (`rust-anything enable` does that), or on its own. Regenerate with `rust-anything refresh`.\n\n\
         {CODEC_PS1}\n\
         if ($null -eq $script:{var}) {{\n    \
         $script:{var} = & $script:RustAnythingCodec.Load (Join-Path $PSScriptRoot $(if ($IsWindows) {{ '{lib}.dll' }} elseif ($IsMacOS) {{ 'lib{lib}.dylib' }} else {{ 'lib{lib}.so' }}))\n\
         }}\n",
        unit = m.unit,
    );
    for f in &m.exports {
        let _ = write!(out, "\nfunction {} {{\n", f.name);
        for line in crate::lang::powershell::param_text(f).lines() {
            let _ = writeln!(out, "    {}", line.trim_end());
        }
        let _ = writeln!(
            out,
            "    $rustAnythingArgs = [object[]]::new({})",
            f.params.len()
        );
        for (i, p) in f.params.iter().enumerate() {
            let _ = writeln!(out, "    $rustAnythingArgs[{i}] = ${{{}}}", p.name);
        }
        let _ = writeln!(
            out,
            "    & $script:RustAnythingCodec.Call $script:{var} '{}' $rustAnythingArgs\n}}",
            f.name.replace('\'', "''")
        );
    }
    out
}

/// Each export's parameter names in order, so the driver can pass recorded arguments by name.
pub fn targets(m: &Manifest) -> serde_json::Value {
    serde_json::Value::Object(
        m.exports
            .iter()
            .map(|f| {
                (
                    f.name.clone(),
                    serde_json::json!({
                        "params": f.params.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
                    }),
                )
            })
            .collect(),
    )
}

pub fn driver() -> String {
    format!("{DRIVER_HEAD}\n{CODEC_PS1}\n{DRIVER_PS1}")
}

const DRIVER_HEAD: &str = r#"param(
    [string]$Mode,
    [string]$TargetsPath,
    [string]$CasesPath,
    [string]$OutPath,
    [int]$MinMs,
    [Parameter(ValueFromRemainingArguments)][string[]]$Files
)
"#;

const DRIVER_PS1: &str = r#"$codec = $script:RustAnythingCodec
$utf8 = [System.Text.UTF8Encoding]::new($false)
$targets = & $codec.Parse ([System.IO.File]::ReadAllText($TargetsPath))
foreach ($file in $Files) {
    if ($file.EndsWith('.psm1', [StringComparison]::OrdinalIgnoreCase)) {
        Import-Module -Name $file -Force -Global -DisableNameChecking -ErrorAction Stop
    } else {
        . $file
    }
}
$commands = @{}
foreach ($name in $targets.Keys) {
    $commands[$name] = Get-Command -Name $name -CommandType Function -ErrorAction Stop | Select-Object -First 1
}

function RustAnythingSplat($Target, $Arguments) {
    $splat = [ordered]@{}
    $params = $Target['params']
    for ($i = 0; $i -lt $Arguments.Count -and $i -lt $params.Count; $i++) {
        $a = $Arguments[$i]
        if ($a -is [System.Collections.IDictionary] -and $a.Count -eq 1 -and $a.Contains('$undefined')) { continue }
        $splat[$params[$i]] = & $codec.Unmark $a
    }
    $splat
}

function RustAnythingInvoke($Command, $Splat) {
    $text = [System.Text.StringBuilder]::new()
    $result = [ordered]@{}
    try {
        $all = @(& $Command @Splat 2>&1 3>&1 6>&1)
        $items = [System.Collections.Generic.List[object]]::new()
        foreach ($o in $all) {
            if ($o -is [System.Management.Automation.InformationRecord]) {
                [void]$text.Append([string]$o.MessageData).Append("`n")
            } elseif ($o -is [System.Management.Automation.WarningRecord]) {
                [void]$text.Append('WARNING: ').Append($o.Message).Append("`n")
            } elseif ($o -is [System.Management.Automation.ErrorRecord]) {
                [void]$text.Append('ERROR: ').Append($o.ToString()).Append("`n")
            } else {
                $items.Add($o)
            }
        }
        if ($items.Count -eq 0) {
            $value = $null
        } elseif ($items.Count -eq 1) {
            $value = $items[0]
        } else {
            $value = $items.ToArray()
        }
        $result['ok'] = & $codec.Json $value
    } catch {
        $result['error'] = & $codec.String $_.Exception.Message
        $result['error_type'] = & $codec.String $_.Exception.GetType().FullName
    }
    if ($text.Length -gt 0) { $result['output'] = & $codec.String $text.ToString() }
    $result
}

function RustAnythingFields($Fields) {
    $parts = foreach ($k in $Fields.Keys) { (& $codec.String $k) + ':' + $Fields[$k] }
    '{' + ($parts -join ',') + '}'
}

function RustAnythingCases([string]$Path) {
    foreach ($line in [System.IO.File]::ReadLines($Path)) {
        if ($line.Trim() -ne '') { , (& $codec.Parse $line) }
    }
}

function RustAnythingExpected($Case) {
    $fields = [ordered]@{}
    foreach ($k in 'ok', 'error', 'error_type', 'output') {
        if ($Case.Contains($k)) { $fields[$k] = & $codec.Json $Case[$k] }
    }
    $fields
}

function RustAnythingSame($Expected, $Got) {
    if ($Got.Contains('error') -and $Got['error'].StartsWith('"Rust panic:')) { return $false }
    $eo = if ($Expected.Contains('output')) { $Expected['output'] } else { '""' }
    $go = if ($Got.Contains('output')) { $Got['output'] } else { '""' }
    if ($eo -cne $go) { return $false }
    if ($Expected.Contains('error')) {
        return $Got.Contains('error') -and $Expected['error_type'] -ceq $Got['error_type']
    }
    -not $Got.Contains('error') -and $Expected['ok'] -ceq $Got['ok']
}

switch ($Mode) {
    'record' {
        $progress = $env:RUST_ANYTHING_PROGRESS
        $writer = [System.IO.StreamWriter]::new($OutPath, $false, $utf8)
        $writer.AutoFlush = $true
        $cases = & $codec.Parse ([System.IO.File]::ReadAllText($CasesPath))
        for ($index = 0; $index -lt $cases.Count; $index++) {
            $c = $cases[$index]
            if ($progress) { [System.IO.File]::AppendAllText($progress, "$index`n") }
            $name = [string]$c['fn']
            $arguments = @()
            if ($c.Contains('args')) { $arguments = $c['args'] }
            $got = RustAnythingInvoke $commands[$name] (RustAnythingSplat $targets[$name] $arguments)
            $line = [ordered]@{ fn = & $codec.String $name; args = & $codec.Json $arguments }
            foreach ($k in $got.Keys) { $line[$k] = $got[$k] }
            $writer.WriteLine((RustAnythingFields $line))
        }
        $writer.Close()
    }
    'diff' {
        $total = 0
        $passed = 0
        $mismatches = [System.Collections.Generic.List[string]]::new()
        foreach ($c in RustAnythingCases $CasesPath) {
            $total++
            $name = [string]$c['fn']
            $arguments = @()
            if ($c.Contains('args')) { $arguments = $c['args'] }
            $got = RustAnythingInvoke $commands[$name] (RustAnythingSplat $targets[$name] $arguments)
            $expected = RustAnythingExpected $c
            if (RustAnythingSame $expected $got) {
                $passed++
            } else {
                $m = [ordered]@{
                    fn = & $codec.String $name
                    args = & $codec.Json $arguments
                    expected = RustAnythingFields $expected
                    actual = RustAnythingFields $got
                }
                $mismatches.Add((RustAnythingFields $m))
            }
        }
        $summary = '{"total":' + $total + ',"passed":' + $passed + ',"failed":' + ($total - $passed) +
            ',"missing":[],"mismatches":[' + ($mismatches -join ',') + ']}'
        [System.IO.File]::WriteAllText($OutPath, $summary, $utf8)
    }
    'bench' {
        $groups = [ordered]@{}
        foreach ($c in RustAnythingCases $CasesPath) {
            $name = [string]$c['fn']
            if (-not $groups.Contains($name)) { $groups[$name] = [System.Collections.Generic.List[object]]::new() }
            $arguments = @()
            if ($c.Contains('args')) { $arguments = $c['args'] }
            $groups[$name].Add((RustAnythingSplat $targets[$name] $arguments))
        }
        $parts = [System.Collections.Generic.List[string]]::new()
        foreach ($name in $groups.Keys) {
            $command = $commands[$name]
            $prepared = $groups[$name]
            $once = {
                foreach ($splat in $prepared) {
                    try { $null = & $command @splat 2>$null 3>$null 6>$null } catch { }
                }
            }
            & $once
            $passes = 0
            $watch = [System.Diagnostics.Stopwatch]::StartNew()
            while ($true) {
                & $once
                $passes++
                if ($watch.Elapsed.TotalMilliseconds -ge $MinMs) { break }
            }
            $ns = $watch.Elapsed.TotalMilliseconds * 1e6 / ($passes * $prepared.Count)
            $parts.Add((& $codec.String $name) + ':{"cases":' + $prepared.Count + ',"ns":' + $ns.ToString('R', [cultureinfo]::InvariantCulture) + '}')
        }
        [System.IO.File]::WriteAllText($OutPath, '{' + ($parts -join ',') + '}', $utf8)
    }
}
"#;

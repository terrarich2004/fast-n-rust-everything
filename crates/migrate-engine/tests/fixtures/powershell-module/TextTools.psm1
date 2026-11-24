# A small module of text helpers, used by the PowerShell tests.

$script:Separator = '-'

function ConvertTo-Slug {
    [CmdletBinding()]
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [string]$Text,
        [string]$Separator = '-',
        [int]$MaxLength = 0
    )
    $slug = ($Text.ToLowerInvariant() -replace '[^a-z0-9]+', $Separator).Trim($Separator)
    if ($MaxLength -gt 0 -and $slug.Length -gt $MaxLength) {
        $slug = $slug.Substring(0, $MaxLength)
    }
    return $slug
}

function Get-WordCount {
    param([string]$Text)
    $counts = [ordered]@{}
    foreach ($word in $Text -split '\s+') {
        if ($word -eq '') { continue }
        $key = $word.ToLowerInvariant()
        if ($counts.Contains($key)) { $counts[$key] += 1 } else { $counts[$key] = 1 }
    }
    $counts
}

function Measure-Mean {
    [OutputType([double])]
    param([double[]]$Values)
    if ($Values.Count -eq 0) {
        throw [System.ArgumentException]::new('mean of nothing')
    }
    $sum = 0.0
    foreach ($v in $Values) { $sum += $v }
    $sum / $Values.Count
}

function Get-Initials {
    param([string[]]$Names, [switch]$Upper)
    $out = foreach ($n in $Names) {
        if ($n) { $n.Substring(0, 1) }
    }
    if ($Upper) { $out = $out | ForEach-Object { $_.ToUpperInvariant() } }
    $out
}

function Test-Equal {
    [OutputType([bool])]
    param([string]$A, [string]$B)
    $A -eq $B
}

function Get-Setting {
    param([hashtable]$Settings, [string]$Name, $Default = $null)
    if ($Settings.ContainsKey($Name)) { $Settings[$Name] } else { $Default }
}

function Get-Lines {
    param([Parameter(ValueFromPipeline)][string]$Line)
    process { $Line.Trim() }
}

function Format-Stamp {
    param([datetime]$At)
    $At.ToString('o')
}

function Invoke-Twice {
    param([scriptblock]$Action)
    & $Action
    & $Action
}

Export-ModuleMember -Function ConvertTo-Slug, Get-WordCount, Measure-Mean, Get-Initials, Test-Equal, Get-Setting, Get-Lines, Format-Stamp, Invoke-Twice

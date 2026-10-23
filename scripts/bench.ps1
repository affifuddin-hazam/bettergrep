# Compare `bettergrep` with ripgrep on a folder of your choice.
#
#   .\scripts\bench.ps1 -Corpus "$env:USERPROFILE\.cargo\registry\src"
#
# Each search runs once to warm the file cache, then $Runs times; the median is
# reported. Both tools print every matching line, and the line counts must agree.
param(
    [Parameter(Mandatory)] [string] $Corpus,
    [string] $Bettergrep = "bettergrep",
    [string] $Ripgrep = "rg",
    [int] $Runs = 5
)

foreach ($tool in $Bettergrep, $Ripgrep) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "$tool not found on PATH" }
}

function Median([scriptblock] $Search) {
    & $Search | Out-Null
    $times = 1..$Runs | ForEach-Object { (Measure-Command { & $Search | Out-Null }).TotalMilliseconds }
    ($times | Sort-Object)[[int][math]::Floor($Runs / 2)]
}

$cases = @(
    @{ Name = 'literal';          Rg = { & $Ripgrep -n --no-heading -F 'unsafe impl Send' $Corpus }; Bg = { & $Bettergrep -F 'unsafe impl Send' $Corpus 2>$null } },
    @{ Name = 'regex';            Rg = { & $Ripgrep -n --no-heading 'fn \w+_unchecked\(' $Corpus }; Bg = { & $Bettergrep 'fn \w+_unchecked\(' $Corpus 2>$null } },
    @{ Name = 'ignore case, -l';  Rg = { & $Ripgrep -i -l 'deprecated' $Corpus };                   Bg = { & $Bettergrep -i -l 'deprecated' $Corpus 2>$null } },
    @{ Name = 'no match';         Rg = { & $Ripgrep -F 'zzqqxx_not_present' $Corpus };              Bg = { & $Bettergrep -F 'zzqqxx_not_present' $Corpus 2>$null } }
)

"{0} | {1}" -f (& $Ripgrep --version | Select-Object -First 1), (& $Bettergrep --version)
"corpus: $Corpus"
""
"{0,-16} {1,12} {2,14} {3,9}" -f "search", "ripgrep", "bettergrep", "lines"
foreach ($c in $cases) {
    $rgLines = @(& $c.Rg).Count
    $bgLines = @(& $c.Bg).Count
    $same = if ($rgLines -eq $bgLines) { "$bgLines" } else { "DIFFER: rg $rgLines / bettergrep $bgLines" }
    "{0,-16} {1,9:N0} ms {2,11:N0} ms {3,9}" -f $c.Name, (Median $c.Rg), (Median $c.Bg), $same
}

exit 0

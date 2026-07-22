# Prep unsupported audio into testdata/, then run funkot-autodj.exe.
# Usage: .\work.ps1 <music-dir> [funkot-autodj args...]
#        .\work.ps1 --self-check
# If -l/--list is omitted, builds testdata/work_playlist.txt (basename sort)
# from supported files in music-dir plus converted FLAC in testdata/.
# Requires: dist/windows-x64/funkot-autodj.exe + MinGW DLLs from ./cross-build.sh
#           (and ffmpeg on PATH for conversion).
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
$Testdata = Join-Path $Root 'testdata'
$Exe = Join-Path $Root 'dist\windows-x64\funkot-autodj.exe'

$Supported = @('mp3', 'm4a', 'aac', 'flac', 'ogg', 'oga', 'wav')
$Audio = $Supported + @('wma', 'aiff', 'aif', 'aifc', 'opus', 'ape', 'wv', 'mpc', 'caf', 'ac3', 'dts', 'tak', 'tta')

function Test-Supported([string]$Ext) { $Supported -contains $Ext.ToLowerInvariant() }
function Test-Audio([string]$Ext) { $Audio -contains $Ext.ToLowerInvariant() }

function Test-HasListArg {
    param([string[]]$Args)
    foreach ($a in $Args) {
        if ($a -eq '-l' -or $a -eq '--list' -or $a -like '--list=*') { return $true }
    }
    return $false
}

if ($args.Count -ge 1 -and $args[0] -eq '--self-check') {
    if ((Test-Supported 'flac') -and -not (Test-Supported 'wma') -and (Test-Audio 'wma')) { }
    else { Write-Error 'fail: ext checks'; exit 1 }
    if (Test-HasListArg @('--render', 'out.wav')) { Write-Error 'fail: false positive list'; exit 1 }
    if (-not (Test-HasListArg @('-l', 'x.txt', '--render', 'out.wav'))) { Write-Error 'fail: missed -l'; exit 1 }
    if (-not (Test-HasListArg @('--list=x.txt'))) { Write-Error 'fail: missed --list='; exit 1 }
    if (-not (Test-Path -LiteralPath $Exe)) { Write-Error "fail: missing exe: $Exe"; exit 1 }
    foreach ($dll in @('libstdc++-6.dll', 'libgcc_s_seh-1.dll', 'libwinpthread-1.dll')) {
        $p = Join-Path (Split-Path -Parent $Exe) $dll
        if (-not (Test-Path -LiteralPath $p)) { Write-Error "fail: missing $dll (re-run ./cross-build.sh)"; exit 1 }
    }
    Write-Output 'ok'
    exit 0
}

if ($args.Count -lt 1) {
    Write-Error "usage: $($MyInvocation.MyCommand.Name) <music-dir> [args...]"
    exit 2
}

$Dir = $args[0]
$Rest = @()
if ($args.Count -gt 1) { $Rest = $args[1..($args.Count - 1)] }

if (-not (Test-Path -LiteralPath $Dir -PathType Container)) {
    Write-Error "not a directory: $Dir"
    exit 1
}
$Dir = (Resolve-Path -LiteralPath $Dir).Path

if (-not (Test-Path -LiteralPath $Exe)) {
    Write-Error "missing exe (build first): $Exe"
    exit 1
}
foreach ($dll in @('libstdc++-6.dll', 'libgcc_s_seh-1.dll', 'libwinpthread-1.dll')) {
    $p = Join-Path (Split-Path -Parent $Exe) $dll
    if (-not (Test-Path -LiteralPath $p)) {
        Write-Error "missing $p (re-run ./cross-build.sh — MinGW runtime required next to the exe)"
        exit 1
    }
}

New-Item -ItemType Directory -Force -Path $Testdata | Out-Null

$Tracks = [System.Collections.Generic.List[string]]::new()
Get-ChildItem -LiteralPath $Dir -File | ForEach-Object {
    $ext = $_.Extension.TrimStart('.').ToLowerInvariant()
    if (-not (Test-Audio $ext)) { return }
    if (Test-Supported $ext) {
        $Tracks.Add($_.FullName)
        return
    }
    $out = Join-Path $Testdata ($_.BaseName + '.flac')
    if (Test-Path -LiteralPath $out) {
        Write-Host "skip (exists): $out" -ForegroundColor DarkYellow
    } else {
        Write-Host "convert: $($_.FullName) -> $out" -ForegroundColor DarkYellow
        & ffmpeg -nostdin -hide_banner -loglevel error -n -i $_.FullName $out
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    $Tracks.Add($out)
}

$CliArgs = [System.Collections.Generic.List[string]]::new()
foreach ($a in $Rest) { $CliArgs.Add([string]$a) }

if (-not (Test-HasListArg $CliArgs.ToArray())) {
    $PlRel = 'testdata\work_playlist.txt'
    $Pl = Join-Path $Root $PlRel
    $sorted = @($Tracks | Sort-Object { [IO.Path]::GetFileName($_) })
    $utf8 = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllLines($Pl, $sorted, $utf8)
    Write-Host "auto playlist: $PlRel ($($Tracks.Count) tracks)" -ForegroundColor DarkYellow
    $CliArgs.Insert(0, $Pl)
    $CliArgs.Insert(0, '-l')
}

Push-Location $Root
try {
    & $Exe @($CliArgs.ToArray())
    $code = $LASTEXITCODE
    # 0xC0000135 STATUS_DLL_NOT_FOUND — exe never reached main (silent on console)
    if ($code -eq -1073741515 -or $code -eq 0xC0000135) {
        Write-Error "exe failed to start (missing DLL). Ensure MinGW runtimes sit next to $Exe (re-run ./cross-build.sh)"
        exit 1
    }
    exit $code
} finally {
    Pop-Location
}

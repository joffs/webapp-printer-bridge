<#
.SYNOPSIS
    Build the release for x64 and/or ARM64 and assemble dist\printer-bridge-<version>-win-<arch>\
    plus a zip of it.

.DESCRIPTION
    Each package contains printer-bridge.exe, the matching pdfium.dll, README.md, LICENSE and
    the PDFium licenses. PDFium is fetched first if missing (scripts\fetch-pdfium.ps1). The script checks
    that exe and DLL are built for the package's architecture and, when dumpbin is available,
    lists their DLL imports.
#>
param(
    [ValidateSet('x64', 'arm64')]
    [string[]]$Arch = @('x64', 'arm64')
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$targets = @{ 'x64' = 'x86_64-pc-windows-msvc'; 'arm64' = 'aarch64-pc-windows-msvc' }
$machines = @{ 'x64' = 0x8664; 'arm64' = 0xAA64 }

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value

function Get-PeMachine([string]$Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    $pe = [BitConverter]::ToInt32($bytes, 0x3C)
    [BitConverter]::ToUInt16($bytes, $pe + 4)
}

function Find-Dumpbin {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $null }
    $vs = & $vswhere -latest -products * -property installationPath
    if (-not $vs) { return $null }
    $hostDir = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'Hostarm64\arm64' } else { 'Hostx64\x64' }
    Get-ChildItem "$vs\VC\Tools\MSVC\*\bin\$hostDir\dumpbin.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName | Select-Object -Last 1 -ExpandProperty FullName
}

& (Join-Path $PSScriptRoot 'fetch-pdfium.ps1') -Arch $Arch
$dumpbin = Find-Dumpbin

Push-Location $root
try {
    foreach ($a in $Arch) {
        $target = $targets[$a]
        Write-Host "== $a ($target)"
        cargo build --release --target $target
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $target" }

        $name = "printer-bridge-$version-win-$a"
        $dir = Join-Path $root "dist\$name"
        if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
        New-Item -ItemType Directory -Force "$dir\licenses\pdfium" | Out-Null

        $vendor = Join-Path $root "vendor\pdfium\$a"
        Copy-Item "target\$target\release\printer-bridge.exe" $dir
        Copy-Item "$vendor\bin\pdfium.dll" $dir
        Copy-Item README.md $dir
        if (Test-Path LICENSE) { Copy-Item LICENSE $dir }
        Copy-Item "$vendor\LICENSE" "$dir\licenses\pdfium\LICENSE"
        Copy-Item "$vendor\licenses\*" "$dir\licenses\pdfium\"

        foreach ($file in 'printer-bridge.exe', 'pdfium.dll') {
            $machine = Get-PeMachine (Join-Path $dir $file)
            if ($machine -ne $machines[$a]) {
                throw ("{0} is machine type 0x{1:X4}, expected 0x{2:X4}" -f $file, $machine, $machines[$a])
            }
            if ($dumpbin) {
                $imports = & $dumpbin /nologo /dependents (Join-Path $dir $file) |
                    Where-Object { $_ -match '^\s+\S+\.dll\s*$' } | ForEach-Object { $_.Trim() }
                Write-Host "  $file imports: $($imports -join ', ')"
            }
        }

        $zip = Join-Path $root "dist\$name.zip"
        if (Test-Path $zip) { Remove-Item -Force $zip }
        Compress-Archive -Path "$dir\*" -DestinationPath $zip
        Write-Host "  -> $zip ($([math]::Round((Get-Item $zip).Length / 1MB, 2)) MB)"
    }
} finally {
    Pop-Location
}

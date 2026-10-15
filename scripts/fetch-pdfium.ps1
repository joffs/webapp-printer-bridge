<#
.SYNOPSIS
    Download the pinned PDFium binaries (bblanchon/pdfium-binaries) into vendor\pdfium\<arch>\.

.DESCRIPTION
    The release tag must match the pdfium-render API feature in Cargo.toml (pdfium_7881).
    Archives are verified against the SHA-256 pinned below. Already fetched architectures are
    skipped unless -Force is given. vendor\ is git-ignored.
#>
param(
    [ValidateSet('x64', 'arm64')]
    [string[]]$Arch = @('x64', 'arm64'),
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$Release = 'chromium/7881'
$Assets = @{
    'x64'   = @{ Name = 'pdfium-win-x64.tgz';   Sha256 = '73CC0DE638AC2095E7445BF56A38200A5B7C7CA0E9F4BA144598F2457377AC08' }
    'arm64' = @{ Name = 'pdfium-win-arm64.tgz'; Sha256 = 'D3035D4D2CACAC6ECD1A2ECE197A3D702A1B2A58466276B9F870B8CB278A9D84' }
}

$root = Split-Path -Parent $PSScriptRoot
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

foreach ($a in $Arch) {
    $asset = $Assets[$a]
    $dest = Join-Path $root "vendor\pdfium\$a"
    $stamp = Join-Path $dest '.release'
    if (-not $Force -and (Test-Path $stamp) -and ((Get-Content $stamp -Raw).Trim() -eq "$Release $($asset.Sha256)")) {
        Write-Host "$a`: $Release already present"
        continue
    }

    $url = "https://github.com/bblanchon/pdfium-binaries/releases/download/$Release/$($asset.Name)"
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString() + '-' + $asset.Name)
    try {
        # GitHub downloads reset the connection now and then: retry.
        for ($try = 1; ; $try++) {
            try {
                Write-Host "$a`: downloading $url"
                Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
                break
            } catch {
                if ($try -ge 5) { throw }
                Write-Warning "Attempt $try failed: $($_.Exception.Message)"
                Start-Sleep -Seconds (2 * $try)
            }
        }

        $hash = (Get-FileHash -Algorithm SHA256 $tmp).Hash
        if ($hash -ne $asset.Sha256) {
            throw "$($asset.Name): SHA-256 mismatch (expected $($asset.Sha256), got $hash)"
        }

        if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
        New-Item -ItemType Directory -Force $dest | Out-Null
        & "$env:SystemRoot\System32\tar.exe" -xzf $tmp -C $dest
        if ($LASTEXITCODE -ne 0) { throw "tar failed with exit code $LASTEXITCODE" }
        if (-not (Test-Path (Join-Path $dest 'bin\pdfium.dll'))) { throw "bin\pdfium.dll missing in $($asset.Name)" }
        Set-Content -Path $stamp -Value "$Release $($asset.Sha256)" -Encoding ASCII
        Write-Host "$a`: extracted to $dest"
    } finally {
        if (Test-Path $tmp) { Remove-Item -Force $tmp }
    }
}

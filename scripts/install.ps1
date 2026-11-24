# Install the latest rrover release on Windows (experimental):
#   irm https://github.com/danik-tro/redis-rover/releases/latest/download/install.ps1 | iex
#
# $env:RROVER_INSTALL_DIR  where to put the binary (default: %LOCALAPPDATA%\rrover\bin)
# $env:RROVER_VERSION      a release tag instead of the latest (e.g. v0.1.0)
$ErrorActionPreference = 'Stop'

$releases = if ($env:RROVER_RELEASES) { $env:RROVER_RELEASES } else { 'https://github.com/danik-tro/redis-rover/releases' }
$installDir = if ($env:RROVER_INSTALL_DIR) { $env:RROVER_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'rrover\bin' }

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
    throw "rrover install: unsupported CPU $env:PROCESSOR_ARCHITECTURE; see $releases"
}

$tag = $env:RROVER_VERSION
if (-not $tag) {
    $response = Invoke-WebRequest -UseBasicParsing -Uri "$releases/latest"
    $url = $response.BaseResponse.ResponseUri
    if (-not $url) { $url = $response.BaseResponse.RequestMessage.RequestUri }
    $tag = ($url.AbsoluteUri -split '/tag/')[1]
    if (-not $tag) { throw "rrover install: no release found at $releases" }
}

$name = "rrover-$tag-windows-x86_64"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $archive = Join-Path $tmp "$name.tar.gz"
    $sums = Join-Path $tmp "$name.sha256"
    Invoke-WebRequest -UseBasicParsing -Uri "$releases/download/$tag/$name.tar.gz" -OutFile $archive
    Invoke-WebRequest -UseBasicParsing -Uri "$releases/download/$tag/$name.sha256" -OutFile $sums

    $expected = ((Get-Content $sums -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
    if ($expected -notmatch '^[0-9a-f]{64}$') { throw "rrover install: $name.sha256 holds no checksum; nothing was installed" }
    $actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
    if ($expected -ne $actual) { throw "rrover install: sha256 mismatch for $name.tar.gz; nothing was installed" }

    tar -xzf $archive -C $tmp rrover.exe
    if ($LASTEXITCODE -ne 0) { throw 'rrover install: the archive holds no rrover.exe' }
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Move-Item -Force (Join-Path $tmp 'rrover.exe') (Join-Path $installDir 'rrover.exe')
    Write-Host "Installed rrover $tag to $installDir\rrover.exe"
    if (($env:PATH -split ';') -notcontains $installDir) { Write-Host "Add $installDir to your PATH to run it." }
} finally {
    Remove-Item -Recurse -Force $tmp
}

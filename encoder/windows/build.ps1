# Windows equivalent of encoder/build.sh: pinned llama.cpp + sha256-verified patches, static, no curl.
# Run from an x64 MSVC developer environment (cuda-build.cmd / clang-build.cmd set one up).
#
#   build.ps1 -Backend cuda|cpu [-CudaArch "75-virtual;80-virtual;86-real;89-real;120a-real"]
#             [-Source DIR] [-BuildDir DIR] [-Out DIR] [-Jobs N] [-Toolchain FILE]
#
# -Toolchain: a CMake toolchain file, e.g. llama.cpp's cmake/x64-windows-llvm.cmake for a clang CPU build.
param(
  [ValidateSet("cuda", "cpu")][string]$Backend = "cuda",
  [string]$CudaArch = "75-virtual;80-virtual;86-real;89-real;120a-real",
  [string]$Source = "",
  [string]$BuildDir = "",
  [string]$Out = "",
  [int]$Jobs = [Environment]::ProcessorCount,
  [string]$Toolchain = ""
)
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$repo = (Resolve-Path "$here\..\..").Path
if (-not $Source) { $Source = "$repo\encoder\build\llama.cpp" }
if (-not $BuildDir) { $BuildDir = "$repo\encoder\build\win-$Backend" }
if (-not $Out) { $Out = "$repo\encoder\build\dist-win-$Backend" }

$pin = Get-Content "$repo\release\encoder-source.json" -Raw | ConvertFrom-Json
foreach ($p in $pin.patches.PSObject.Properties) {
  $h = (Get-FileHash "$repo\encoder\patches\$($p.Name)" -Algorithm SHA256).Hash.ToLower()
  if ($h -ne $p.Value) { throw "patch $($p.Name) has sha256 $h, pinned $($p.Value)" }
}
if (-not (Test-Path "$Source\.git")) {
  New-Item -ItemType Directory -Force $Source | Out-Null
  git -C $Source init -q; git -C $Source remote add origin $pin.repository
  git -C $Source fetch -q --depth 1 origin $pin.commit
  git -C $Source -c advice.detachedHead=false checkout -q FETCH_HEAD
  foreach ($p in $pin.patches.PSObject.Properties) {
    git -C $Source apply "$repo\encoder\patches\$($p.Name)"
    if ($LASTEXITCODE) { throw "git apply $($p.Name) failed" }
  }
}
$head = git -C $Source rev-parse HEAD
if ($head -ne $pin.commit) { throw "$Source is at $head, pinned $($pin.commit)" }

# Static CRT (/MT, no VC++ redist), an explicit AVX2 baseline (GGML_NATIVE=OFF alone gives an SSE-only CPU backend),
# ggml's own threadpool (no OpenMP DLL), cudart_static. Windows has no static cuBLAS: its DLLs ship beside the exe.
$flags = @(
  "-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release", "-DLLAMA_SRC=$Source", "-DENCODER_DIR=$repo\encoder",
  "-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded",
  "-DGGML_NATIVE=OFF", "-DGGML_AVX=ON", "-DGGML_AVX2=ON", "-DGGML_FMA=ON", "-DGGML_F16C=ON",
  "-DGGML_OPENMP=OFF", "-DGGML_STATIC=ON", "-DARC_MANIFEST=ON"
)
if ($Backend -eq "cuda") { $flags += @("-DGGML_CUDA=ON", "-DCMAKE_CUDA_ARCHITECTURES=$CudaArch") }
else { $flags += "-DGGML_CUDA=OFF" }
if ($Toolchain) { $flags += "-DCMAKE_TOOLCHAIN_FILE=$Toolchain" }

$t = Get-Date
cmake -S $here -B $BuildDir @flags
if ($LASTEXITCODE) { throw "configure failed" }
cmake --build $BuildDir --target rayline-arc-encoder -j $Jobs
if ($LASTEXITCODE) { throw "build failed" }
"build_seconds $([int]((Get-Date) - $t).TotalSeconds)"

New-Item -ItemType Directory -Force $Out | Out-Null
Copy-Item "$BuildDir\rayline-arc-encoder.exe" $Out -Force
if ($Backend -eq "cuda") {
  $cudaBin = Join-Path $env:CUDA_PATH "bin"
  foreach ($dll in "cublas64_12.dll", "cublasLt64_12.dll") { Copy-Item (Join-Path $cudaBin $dll) $Out -Force }
}
Get-ChildItem $Out | Select-Object Name, Length

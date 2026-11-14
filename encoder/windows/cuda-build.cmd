@echo off
rem CUDA build with MSVC. Needs VS 2022 Build Tools (VCTools), CMake, Ninja, Git, and the CUDA toolkit
rem unpacked from NVIDIA's redist zips into %CUDA_PATH% (see README.md).
rem   cuda-build.cmd [extra build.ps1 arguments, e.g. -CudaArch 89 -Jobs 8]
setlocal
if not defined CUDA_PATH (echo set CUDA_PATH to the CUDA toolkit root & exit /b 1)
for /f "usebackq delims=" %%i in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set VSDIR=%%i
call "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
set CUDAToolkit_ROOT=%CUDA_PATH%
set PATH=%CUDA_PATH%\bin;%PATH%
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" -Backend cuda %*

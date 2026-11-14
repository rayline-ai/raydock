@echo off
rem CPU-only build with clang (llama.cpp's x64-windows-llvm toolchain), ~1.6x faster than MSVC on CPU.
rem Needs LLVM on PATH plus the CMake/Ninja/Git/VS Build Tools that cuda-build.cmd needs.
rem   clang-build.cmd [extra build.ps1 arguments]
setlocal
for /f "usebackq delims=" %%i in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set VSDIR=%%i
call "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
set SRC=%~dp0..\build\llama.cpp
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" -Backend cpu -Toolchain "%SRC%\cmake\x64-windows-llvm.cmake" -BuildDir "%~dp0..\build\win-cpu-clang" %*

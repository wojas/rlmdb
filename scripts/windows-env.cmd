@echo off
rem Toolchain setup for scripts/windows-test.sh: copy this file to the work
rem directory on the Windows machine as env.cmd (or name it with
rem WINDOWS_ENV). It is `call`ed from that directory before every command
rem and keeps everything under the directory it lives in:
rem
rem   rustup\  cargo\   rustup-init.exe, run with RUSTUP_HOME and CARGO_HOME
rem                     set as below, with the stable toolchain for the host
rem                     (x86_64-pc-windows-msvc) and
rem                     `rustup toolchain install stable-x86_64-pc-windows-gnu`
rem                     for the MinGW build
rem   msys64\           MSYS2, with `pacman -S mingw-w64-x86_64-gcc` for the
rem                     C reference library and the GNU target
rem   target\           cargo's build directory and the tests' scratch
rem                     (CARGO_TARGET_TMPDIR); test environments there are
rem                     full map-size files on Windows, so delete it between
rem                     sessions, or point CARGO_TARGET_DIR, TEMP and TMP
rem                     at a drive with room (the tests' temporary
rem                     directories follow TEMP)
rem   src\              the checkout, sent by windows-test.sh
rem
rem MSVC needs the Visual Studio Build Tools (C++ workload) on the machine;
rem their vcvars64.bat can be `call`ed here too.
set RUSTUP_HOME=%~dp0rustup
set CARGO_HOME=%~dp0cargo
set CARGO_TARGET_DIR=%~dp0target
set PATH=%~dp0cargo\bin;%~dp0msys64\mingw64\bin;%PATH%

@echo off
setlocal
rem Run from an ARM64 Developer Command Prompt with Visual Studio 17.11 or newer.
if not exist artifacts\windows\arm64x mkdir artifacts\windows\arm64x
pushd artifacts\windows\arm64x
cl /nologo /c /Foempty_arm64.obj ..\..\..\packaging\windows\empty.cpp
if errorlevel 1 goto failure
cl /nologo /c /arm64EC /Foempty_x64.obj ..\..\..\packaging\windows\empty.cpp
if errorlevel 1 goto failure
link /lib /machine:x64 /def:..\..\..\packaging\windows\x64.def /out:forward_x64.lib
if errorlevel 1 goto failure
link /lib /machine:arm64 /def:..\..\..\packaging\windows\arm64.def /out:forward_arm64.lib
if errorlevel 1 goto failure
link /dll /noentry /machine:arm64x /defArm64Native:..\..\..\packaging\windows\arm64.def /def:..\..\..\packaging\windows\x64.def empty_arm64.obj empty_x64.obj /out:kanade_tip.dll forward_arm64.lib forward_x64.lib
if errorlevel 1 goto failure
popd
exit /b 0
:failure
popd
exit /b 1

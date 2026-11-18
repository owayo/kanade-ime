@echo off
setlocal
set "setup=%~dp0artifacts\release\kanade-ime-x86_64-pc-windows-msvc.exe"
if not exist "%setup%" set "setup=%~dp0kanade-ime-x86_64-pc-windows-msvc.exe"
if not exist "%setup%" (
    echo Kanade Setup was not found. Place the Windows release EXE beside install.bat
    echo or build it with .github\scripts\package-windows.ps1.
    exit /b 2
)
start "" /wait "%setup%" /install /passive /norestart
set "status=%errorlevel%"
if "%status%"=="3010" echo Kanade is installed. Restart Windows to finish.
if not "%status%"=="0" exit /b %status%
powershell.exe -NoLogo -NoProfile -ExecutionPolicy RemoteSigned -File "%ProgramW6432%\Kanade\enable-profile.ps1"
if errorlevel 1 exit /b 1
echo Kanade IME is installed. Select Kanade in the Japanese input methods.
set "model=%ProgramW6432%\Kanade\models\kanade-tsumugi-v0.1.0-s-F16.gguf"
if not exist "%model%" (
    echo Bundled Tsumugi model is missing. Repair the installation.
    exit /b 2
)
if /i "%~1"=="--accept-tsumugi-terms" goto configure
echo Tsumugi uses LicenseRef-Kanade-Model-1.0. It processes the reading, candidates
echo and up to 40 preceding characters on this computer.
choice /m "Accept the model terms and local processing and enable Tsumugi"
if not "%errorlevel%"=="1" exit /b 0
:configure
powershell.exe -NoLogo -NoProfile -ExecutionPolicy RemoteSigned -File "%ProgramW6432%\Kanade\enable-tsumugi.ps1" -Model "%model%"
exit /b %errorlevel%

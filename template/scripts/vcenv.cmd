@echo off
setlocal EnableExtensions

if defined INCLUDE if defined LIB goto run

set "VCVARS="
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" goto fallback

for /f "usebackq delims=" %%I in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSINSTALL=%%I"
if not defined VSINSTALL goto fallback
if exist "%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat" if exist "%VSINSTALL%\VC\Auxiliary\Build\vcvarsall.bat" set "VCVARS=%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat"
if defined VCVARS goto load

:fallback
for %%P in (
  "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
  "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
  "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
  "C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvars64.bat"
  "C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvars64.bat"
  "C:\Program Files\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
  "C:\Program Files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat"
  "C:\Program Files\Microsoft Visual Studio\18\Professional\VC\Auxiliary\Build\vcvars64.bat"
  "C:\Program Files\Microsoft Visual Studio\18\Enterprise\VC\Auxiliary\Build\vcvars64.bat"
) do if not defined VCVARS if exist %%~P if exist "%%~dpPvcvarsall.bat" set "VCVARS=%%~P"

if defined VCVARS goto load
echo make: MSVC environment is missing and a working vcvars64.bat was not found. 1>&2
exit /b 1

:load
call "%VCVARS%" >nul
if errorlevel 1 goto loadfail

:run
if not defined CARGO_LINE goto direct
%CARGO_LINE%
goto done

:direct
%*
goto done

:loadfail
echo make: failed to load "%VCVARS%". 1>&2
exit /b 1

:done
exit /b %ERRORLEVEL%
